//! Relay 기기 권한 강제 — **부수효과 이전에** 판정한다.
//!
//! 브라우저 UI에서 버튼을 숨기는 것은 강제가 아니다. 위조된 메시지는 UI를 거치지 않는다.
//! 그래서 Mac 쪽에서 모든 명령을 이 어댑터에 통과시키고, 허용된 것만 런타임 명령·업로드
//! 싱크·승인 저장소로 내보낸다.
//!
//! Relay 자격증명은 protocol-v3 `auth`나 loopback HTTP 라우터로 **절대** 넘어가지 않는다.
//! 두 전송의 인증은 서로를 대체할 수 없다 — 하나가 뚫리면 다른 하나까지 열리는 구조를
//! 만들지 않는다.
//!
//! 취소와 권한 강등은 **살아 있는 채널에 즉시** 반영된다. 다음 접속에서만 확인하면, 취소
//! 버튼을 누른 뒤에도 이미 붙어 있는 기기가 계속 보고 있게 된다.

use crate::protocol::{ClientMsg, ServerMsg};
use crate::relay::contract::{RelayAction, RelayPermissions};

/// 연속 위반 상한. 이 횟수를 넘기면 채널을 닫는다 — 거절만 반복하면 상대가 무한히
/// 두드려 볼 수 있다.
pub const MAX_PERMISSION_VIOLATIONS: u32 = 8;

/// 한 명령에 대한 판정.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayAdmission {
    /// 허용. 호출자는 이 명령만 실행한다.
    Allow(ClientMsg),
    /// 업로드 허용. 업로드는 WS 명령이 아니므로 별도 모양을 갖는다 — 여기에 아무 `ClientMsg`나
    /// 끼워 넣으면, 계약대로 "허용된 명령을 실행"하는 호출자가 엉뚱한 부수효과를 낸다.
    AllowUpload,
    /// 거부. 부수효과 없이 거절하며 사유 코드만 남긴다.
    Denied(DenialReason),
    /// 위반이 상한을 넘었다. 채널을 닫는다.
    CloseChannel(DenialReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// 이 기기에 그 권한이 없다.
    PermissionDenied(RelayAction),
    /// 이 기기의 인가가 취소됐다.
    Revoked,
    /// Relay 채널에 올 수 없는 메시지다(예: protocol-v3 인증).
    NotOnThisTransport,
}

pub struct RelayMessageAdapter {
    permissions: RelayPermissions,
    revoked: bool,
    violations: u32,
    max_violations: u32,
}

impl RelayMessageAdapter {
    pub fn new(permissions: RelayPermissions) -> Self {
        Self {
            permissions,
            revoked: false,
            violations: 0,
            max_violations: MAX_PERMISSION_VIOLATIONS,
        }
    }

    #[cfg(test)]
    fn with_violation_limit(permissions: RelayPermissions, max_violations: u32) -> Self {
        Self {
            permissions,
            revoked: false,
            violations: 0,
            max_violations,
        }
    }

    pub const fn permissions(&self) -> RelayPermissions {
        self.permissions
    }

    pub const fn is_revoked(&self) -> bool {
        self.revoked
    }

    pub const fn violations(&self) -> u32 {
        self.violations
    }

    /// 권한을 갈아 끼운다. 강등은 **이 채널에 바로** 적용된다.
    pub const fn apply_permissions(&mut self, permissions: RelayPermissions) {
        self.permissions = permissions;
    }

    /// 인가 취소. 이후 모든 명령이 거부되고 어떤 서버 메시지도 나가지 않는다.
    pub const fn revoke(&mut self) {
        self.revoked = true;
    }

    /// 이 명령에 필요한 권한. `None`이면 이 전송에 올 수 없는 메시지다.
    const fn required(message: &ClientMsg) -> Option<RelayAction> {
        match message {
            // protocol-v3 페어링 토큰은 loopback 전송의 것이다. Relay 채널에서 이 메시지를
            // 받아 주면 두 인증 체계가 서로를 대체할 수 있게 된다.
            ClientMsg::Auth { .. } => None,
            ClientMsg::Watch { .. } | ClientMsg::Unwatch | ClientMsg::RequestKeyframe => {
                Some(RelayAction::View)
            }
            ClientMsg::Input { .. } | ClientMsg::DirectInput { .. } => Some(RelayAction::Input),
            ClientMsg::Key { .. } | ClientMsg::DirectKey { .. } => Some(RelayAction::Key),
            ClientMsg::Scroll { .. } => Some(RelayAction::Scroll),
            ClientMsg::Switch { .. } => Some(RelayAction::Switch),
            ClientMsg::Resolve { .. } => Some(RelayAction::Approval),
        }
    }

    /// 명령 하나를 판정한다. **어떤 부수효과보다 먼저** 불려야 한다.
    pub fn admit(&mut self, message: ClientMsg) -> RelayAdmission {
        if self.revoked {
            return self.deny(DenialReason::Revoked);
        }
        let Some(action) = Self::required(&message) else {
            return self.deny(DenialReason::NotOnThisTransport);
        };
        if !self.permissions.allows(action) {
            return self.deny(DenialReason::PermissionDenied(action));
        }
        // 허용된 명령은 위반 계수를 건드리지 않는다 — 정상 사용이 상한을 갉아먹으면
        // 오래 쓴 기기가 이유 없이 끊긴다.
        RelayAdmission::Allow(message)
    }

    /// 업로드는 WS 메시지가 아니라 별도 경로로 온다. 그래서 판정도 따로 받는다.
    pub fn admit_upload(&mut self) -> RelayAdmission {
        if self.revoked {
            return self.deny(DenialReason::Revoked);
        }
        if !self.permissions.allows(RelayAction::Upload) {
            return self.deny(DenialReason::PermissionDenied(RelayAction::Upload));
        }
        RelayAdmission::AllowUpload
    }

    /// 이 기기로 내보내도 되는 서버 메시지인가.
    ///
    /// 승인 목록은 별도 권한이다. 첫 릴리스의 view-only 기기는 접속 시점 스냅샷이든 이후
    /// 갱신이든 승인 정보를 **한 번도** 받지 않는다.
    pub const fn may_emit(&self, message: &ServerMsg) -> bool {
        if self.revoked {
            return false;
        }
        match message {
            ServerMsg::Approvals { .. } => self.permissions.allows(RelayAction::Approval),
            ServerMsg::Welcome { .. }
            | ServerMsg::Dashboard { .. }
            | ServerMsg::Viewport { .. }
            | ServerMsg::InputPressure { .. }
            | ServerMsg::Error { .. } => self.permissions.allows(RelayAction::View),
        }
    }

    fn deny(&mut self, reason: DenialReason) -> RelayAdmission {
        self.violations = self.violations.saturating_add(1);
        if self.violations >= self.max_violations {
            RelayAdmission::CloseChannel(reason)
        } else {
            RelayAdmission::Denied(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CursorView, ResourceView};

    fn view_only() -> RelayMessageAdapter {
        RelayMessageAdapter::new(RelayPermissions::default())
    }

    fn privileged_messages() -> Vec<ClientMsg> {
        vec![
            ClientMsg::Input {
                session: "1".to_owned(),
                text: "rm -rf /".to_owned(),
                submit: true,
            },
            ClientMsg::Key {
                session: "1".to_owned(),
                key: "ctrl_c".to_owned(),
            },
            ClientMsg::Scroll {
                session: "1".to_owned(),
                delta: 5,
            },
            ClientMsg::Switch {
                workspace: "other".to_owned(),
            },
            ClientMsg::Resolve {
                id: "approval-1".to_owned(),
                allowed: true,
                remember: true,
            },
        ]
    }

    #[test]
    fn direct_terminal_messages_require_the_existing_input_grant() {
        for message in [
            ClientMsg::DirectInput {
                session: "u7".into(),
                text: "한글".into(),
                paste: false,
            },
            ClientMsg::DirectKey {
                session: "u7".into(),
                key: "c".into(),
                ctrl: true,
                alt: false,
                shift: false,
                meta: false,
            },
        ] {
            assert!(matches!(
                view_only().admit(message.clone()),
                RelayAdmission::Denied(DenialReason::PermissionDenied(_))
            ));
            let mut granted = RelayMessageAdapter::new(RelayPermissions::new(true, true, false));
            assert!(matches!(granted.admit(message), RelayAdmission::Allow(_)));
        }
    }

    fn dashboard() -> ServerMsg {
        ServerMsg::Dashboard {
            workspaces: Vec::new(),
            resource: None::<ResourceView>,
            notice: None,
        }
    }

    fn approvals() -> ServerMsg {
        ServerMsg::Approvals {
            pending: Vec::new(),
        }
    }

    fn viewport() -> ServerMsg {
        ServerMsg::Viewport {
            session: "1".to_owned(),
            seq: 1,
            keyframe: true,
            cols: 80,
            rows: 24,
            cursor: CursorView {
                row: 0,
                col: 0,
                visible: true,
                shape: "block",
            },
            alt: false,
            offset: 0,
            lines: Vec::new(),
        }
    }

    /// 실행된 부수효과를 기록한다. 거부된 명령이 여기 한 줄이라도 남으면 실패다.
    #[derive(Default)]
    struct RecordingExecutor {
        runtime_commands: Vec<String>,
        uploads: usize,
        approval_mutations: Vec<String>,
    }

    impl RecordingExecutor {
        /// 호출자가 실제로 하는 일: 판정이 `Allow`일 때에만 실행한다.
        fn apply(&mut self, admission: &RelayAdmission) {
            let RelayAdmission::Allow(message) = admission else {
                return;
            };
            match message {
                ClientMsg::Input { session, .. }
                | ClientMsg::Key { session, .. }
                | ClientMsg::Scroll { session, .. } => {
                    self.runtime_commands.push(session.clone());
                }
                ClientMsg::Switch { workspace } => self.runtime_commands.push(workspace.clone()),
                ClientMsg::Resolve { id, .. } => self.approval_mutations.push(id.clone()),
                _ => {}
            }
        }

        fn touched_anything(&self) -> bool {
            !self.runtime_commands.is_empty()
                || self.uploads > 0
                || !self.approval_mutations.is_empty()
        }
    }

    #[test]
    fn a_view_only_device_still_receives_dashboard_and_viewport_traffic() {
        let adapter = view_only();
        assert!(adapter.may_emit(&ServerMsg::Welcome { v: 3 }));
        assert!(adapter.may_emit(&dashboard()));
        assert!(adapter.may_emit(&viewport()));
        assert!(adapter.may_emit(&ServerMsg::InputPressure {
            session: "1".to_owned(),
            queued: 0,
            reason: "queue_full",
        }));
    }

    /// 접속 시점 스냅샷이든 이후 갱신이든, view-only 기기는 승인 정보를 한 번도 받지 않는다.
    #[test]
    fn approvals_never_reach_a_view_only_device_at_any_point() {
        let adapter = view_only();
        assert!(!adapter.may_emit(&approvals()), "접속 시점 스냅샷");

        // 시간이 지나 다시 와도 마찬가지다.
        for _ in 0..5 {
            assert!(!adapter.may_emit(&approvals()), "이후 갱신");
        }

        // 승인 권한이 있는 기기에만 나간다.
        let approver = RelayMessageAdapter::new(RelayPermissions::new(true, false, true));
        assert!(approver.may_emit(&approvals()));
    }

    /// 위조된 특권 명령은 **부수효과가 일어나기 전에** 거부된다.
    #[test]
    fn forged_privileged_commands_are_denied_without_touching_any_sink() {
        let mut adapter =
            RelayMessageAdapter::with_violation_limit(RelayPermissions::default(), u32::MAX);
        let mut executor = RecordingExecutor::default();

        for message in privileged_messages() {
            let admission = adapter.admit(message.clone());
            executor.apply(&admission);
            assert!(
                matches!(
                    admission,
                    RelayAdmission::Denied(DenialReason::PermissionDenied(_))
                ),
                "{message:?} 는 거부돼야 한다: {admission:?}"
            );
        }
        assert!(
            !executor.touched_anything(),
            "거부된 명령이 런타임·업로드·승인 저장소를 건드렸다: {:?} {:?} {}",
            executor.runtime_commands,
            executor.approval_mutations,
            executor.uploads
        );

        let upload = adapter.admit_upload();
        assert!(matches!(
            upload,
            RelayAdmission::Denied(DenialReason::PermissionDenied(RelayAction::Upload))
        ));
        assert_eq!(executor.uploads, 0);
    }

    /// 입력 권한을 줘도 업로드는 열리지 않는다. 두 권한은 독립이다.
    #[test]
    fn granting_input_never_implies_upload() {
        let mut adapter = RelayMessageAdapter::with_violation_limit(
            RelayPermissions::new(true, true, false),
            u32::MAX,
        );
        assert!(matches!(
            adapter.admit(ClientMsg::Input {
                session: "1".to_owned(),
                text: "ok".to_owned(),
                submit: false,
            }),
            RelayAdmission::Allow(_)
        ));
        assert!(
            matches!(
                adapter.admit_upload(),
                RelayAdmission::Denied(DenialReason::PermissionDenied(RelayAction::Upload))
            ),
            "입력 권한이 업로드를 함의하면 안 된다"
        );
        assert!(
            !adapter.may_emit(&approvals()),
            "입력 권한이 승인 열람을 함의하면 안 된다"
        );

        // 업로드 권한을 따로 줘야 열린다.
        adapter.apply_permissions(RelayPermissions::new(true, true, false).with_upload(true));
        assert_eq!(adapter.admit_upload(), RelayAdmission::AllowUpload);
    }

    /// 정상 사용은 위반 계수를 갉아먹지 않는다.
    #[test]
    fn allowed_commands_do_not_consume_the_violation_budget() {
        let mut adapter = view_only();
        for _ in 0..(MAX_PERMISSION_VIOLATIONS * 4) {
            assert!(matches!(
                adapter.admit(ClientMsg::RequestKeyframe),
                RelayAdmission::Allow(_)
            ));
        }
        assert_eq!(adapter.violations(), 0);
    }

    /// 거절만 반복하면 상대가 무한히 두드려 볼 수 있다. 상한에서 채널을 닫는다.
    #[test]
    fn repeated_forbidden_commands_close_the_channel_at_a_bounded_count() {
        let mut adapter = view_only();
        let mut closed_at = None;
        for attempt in 1..=(MAX_PERMISSION_VIOLATIONS + 4) {
            let admission = adapter.admit(ClientMsg::Switch {
                workspace: "other".to_owned(),
            });
            if let RelayAdmission::CloseChannel(_) = admission {
                closed_at = Some(attempt);
                break;
            }
        }
        assert_eq!(
            closed_at,
            Some(MAX_PERMISSION_VIOLATIONS),
            "상한에 도달하면 정확히 그때 닫아야 한다"
        );
    }

    /// 취소는 **살아 있는 채널에 즉시** 적용된다. 다음 접속에서만 보면 늦다.
    #[test]
    fn revocation_takes_effect_on_the_live_channel_immediately() {
        let mut adapter = view_only();
        assert!(matches!(
            adapter.admit(ClientMsg::RequestKeyframe),
            RelayAdmission::Allow(_)
        ));
        assert!(adapter.may_emit(&dashboard()));

        adapter.revoke();

        assert!(adapter.is_revoked());
        assert!(
            !adapter.may_emit(&dashboard()) && !adapter.may_emit(&viewport()),
            "취소된 기기에는 화면이 더 나가면 안 된다"
        );
        assert!(matches!(
            adapter.admit(ClientMsg::RequestKeyframe),
            RelayAdmission::Denied(DenialReason::Revoked) | RelayAdmission::CloseChannel(_)
        ));
    }

    /// 권한 강등도 같은 채널에 바로 적용된다.
    #[test]
    fn a_permission_downgrade_applies_to_the_same_open_channel() {
        let mut adapter = RelayMessageAdapter::with_violation_limit(
            RelayPermissions::new(true, true, true).with_upload(true),
            u32::MAX,
        );
        assert!(matches!(
            adapter.admit(ClientMsg::Switch {
                workspace: "other".to_owned()
            }),
            RelayAdmission::Allow(_)
        ));
        assert!(adapter.may_emit(&approvals()));
        assert_eq!(adapter.admit_upload(), RelayAdmission::AllowUpload);

        // 첫 릴리스의 고정 권한으로 되돌린다.
        adapter.apply_permissions(RelayPermissions::default());

        assert!(matches!(
            adapter.admit(ClientMsg::Switch {
                workspace: "other".to_owned()
            }),
            RelayAdmission::Denied(DenialReason::PermissionDenied(RelayAction::Switch))
        ));
        assert!(!adapter.may_emit(&approvals()), "강등이 즉시 반영돼야 한다");
        assert!(matches!(
            adapter.admit_upload(),
            RelayAdmission::Denied(DenialReason::PermissionDenied(RelayAction::Upload))
        ));
        // 시청은 계속 된다 — 강등이지 차단이 아니다.
        assert!(adapter.may_emit(&viewport()));
    }

    /// 업로드 허용은 **자기만의 모양**을 갖는다. 아무 `ClientMsg`나 끼워 돌려주면, 계약대로
    /// "허용된 명령을 실행"하는 호출자가 엉뚱한 부수효과를 낸다.
    #[test]
    fn an_upload_admission_never_masquerades_as_a_client_command() {
        let mut adapter =
            RelayMessageAdapter::new(RelayPermissions::new(true, false, false).with_upload(true));
        let admission = adapter.admit_upload();
        assert_eq!(admission, RelayAdmission::AllowUpload);
        assert!(
            !matches!(admission, RelayAdmission::Allow(_)),
            "업로드 허용이 명령 허용으로 보이면 안 된다"
        );

        let mut executor = RecordingExecutor::default();
        executor.apply(&admission);
        assert!(
            !executor.touched_anything(),
            "업로드 허용이 런타임 명령이나 승인 변경을 일으키면 안 된다"
        );
    }

    /// key/input/scroll/switch가 **한 권한**으로 묶이는 것은 계획의 1차 릴리스 능력표가
    /// 정한 설계다(`docs/superpowers/plans/2026-08-28-production-relay.md`의
    /// "future input grant | key, input, scroll, switch"). 실수로 뭉뚱그린 것이 아니라는
    /// 사실을 여기서 고정한다 — 나눠야 한다면 계획을 먼저 고쳐야 한다.
    #[test]
    fn the_single_input_grant_deliberately_covers_key_scroll_and_switch() {
        let input_granted = RelayPermissions::new(true, true, false);
        for action in [
            RelayAction::Input,
            RelayAction::Key,
            RelayAction::Scroll,
            RelayAction::Switch,
        ] {
            assert!(
                input_granted.allows(action),
                "{action:?} 는 입력 권한에 포함된다 (능력표)"
            );
        }
        // 그러나 업로드와 승인은 끝까지 별개다.
        assert!(!input_granted.allows(RelayAction::Upload));
        assert!(!input_granted.allows(RelayAction::Approval));

        // 1차 릴리스의 기본값은 넷 모두를 거부한다.
        let view_only = RelayPermissions::default();
        for action in [
            RelayAction::Input,
            RelayAction::Key,
            RelayAction::Scroll,
            RelayAction::Switch,
            RelayAction::Upload,
            RelayAction::Approval,
        ] {
            assert!(!view_only.allows(action), "{action:?}");
        }
        assert!(view_only.allows(RelayAction::View));
    }

    /// Relay 채널은 protocol-v3 페어링 토큰을 절대 받지 않는다. 받아 주면 두 인증 체계가
    /// 서로를 대체할 수 있게 된다.
    #[test]
    fn a_protocol_v3_auth_frame_is_never_accepted_over_relay() {
        for permissions in [
            RelayPermissions::default(),
            RelayPermissions::new(true, true, true).with_upload(true),
        ] {
            let mut adapter = RelayMessageAdapter::with_violation_limit(permissions, u32::MAX);
            let admission = adapter.admit(ClientMsg::Auth {
                token: "loopback-pairing-token".to_owned(),
                v: 3,
            });
            assert_eq!(
                admission,
                RelayAdmission::Denied(DenialReason::NotOnThisTransport),
                "권한이 아무리 넓어도 Relay에서 protocol-v3 인증은 받지 않는다"
            );
        }
    }

    /// 이 모듈은 loopback 인증·토큰·라우터를 이름조차 알지 못한다.
    #[test]
    fn the_adapter_never_reaches_into_the_loopback_authentication_path() {
        let source = include_str!("adapter.rs");
        let production = source.split("\n#[cfg(test)]\nmod tests {").next().unwrap();
        for forbidden in [
            "WEB_TOKEN_ID",
            "pairing::",
            "static_srv",
            "http::",
            "allowed_host",
            "get_or_create_token",
        ] {
            assert!(!production.contains(forbidden), "{forbidden}");
        }
    }
}
