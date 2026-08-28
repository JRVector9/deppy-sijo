/// Fixed width used by Relay rendezvous, device, and connection identifiers.
pub const RELAY_ID_BYTES: usize = 16;

macro_rules! opaque_relay_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name([u8; RELAY_ID_BYTES]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; RELAY_ID_BYTES]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; RELAY_ID_BYTES] {
                &self.0
            }
        }
    };
}

opaque_relay_id!(PairingId);
opaque_relay_id!(DeviceId);
opaque_relay_id!(ConnectionId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelayAction {
    View,
    Input,
    Key,
    Scroll,
    Switch,
    Upload,
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayPermissions {
    view: bool,
    input: bool,
    upload: bool,
    approval: bool,
}

impl RelayPermissions {
    pub const fn new(view: bool, input: bool, approval: bool) -> Self {
        Self {
            view,
            input,
            upload: false,
            approval,
        }
    }

    pub const fn with_upload(mut self, upload: bool) -> Self {
        self.upload = upload;
        self
    }

    pub const fn allows(&self, action: RelayAction) -> bool {
        match action {
            RelayAction::View => self.view,
            RelayAction::Input | RelayAction::Key | RelayAction::Scroll | RelayAction::Switch => {
                self.input
            }
            RelayAction::Upload => self.upload,
            RelayAction::Approval => self.approval,
        }
    }
}

impl Default for RelayPermissions {
    fn default() -> Self {
        Self::new(true, false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_permissions_default_to_view_only() {
        let permissions = RelayPermissions::default();

        assert!(permissions.allows(RelayAction::View));
        for action in [
            RelayAction::Input,
            RelayAction::Key,
            RelayAction::Scroll,
            RelayAction::Switch,
            RelayAction::Upload,
            RelayAction::Approval,
        ] {
            assert!(
                !permissions.allows(action),
                "default permissions unexpectedly allow {action:?}"
            );
        }
    }

    #[test]
    fn relay_actions_map_to_exact_permission_grants() {
        let view_only = RelayPermissions::new(true, false, false);
        let input_only = RelayPermissions::new(false, true, false);
        let upload_only = RelayPermissions::new(false, false, false).with_upload(true);
        let approval_only = RelayPermissions::new(false, false, true);

        assert!(view_only.allows(RelayAction::View));
        assert!(!view_only.allows(RelayAction::Input));
        assert!(!view_only.allows(RelayAction::Approval));

        for action in [
            RelayAction::Input,
            RelayAction::Key,
            RelayAction::Scroll,
            RelayAction::Switch,
        ] {
            assert!(
                input_only.allows(action),
                "input grant must allow {action:?}"
            );
        }
        assert!(!input_only.allows(RelayAction::View));
        assert!(!input_only.allows(RelayAction::Upload));
        assert!(!input_only.allows(RelayAction::Approval));

        assert!(upload_only.allows(RelayAction::Upload));
        assert!(!upload_only.allows(RelayAction::View));
        assert!(!upload_only.allows(RelayAction::Input));
        assert!(!upload_only.allows(RelayAction::Approval));

        assert!(approval_only.allows(RelayAction::Approval));
        assert!(!approval_only.allows(RelayAction::View));
        assert!(!approval_only.allows(RelayAction::Input));
        assert!(!approval_only.allows(RelayAction::Upload));
    }

    #[test]
    fn relay_identifiers_are_distinct_opaque_types() {
        let pairing = PairingId::from_bytes([0x11; RELAY_ID_BYTES]);
        let device = DeviceId::from_bytes([0x22; RELAY_ID_BYTES]);
        let connection = ConnectionId::from_bytes([0x33; RELAY_ID_BYTES]);

        fn require_distinct_types(_: PairingId, _: DeviceId, _: ConnectionId) {}
        require_distinct_types(pairing, device, connection);

        let production = include_str!("contract.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production contract source");
        assert!(!production.contains("pub struct PairingId(pub"));
        assert!(!production.contains("pub struct DeviceId(pub"));
        assert!(!production.contains("pub struct ConnectionId(pub"));
    }

    #[test]
    fn relay_identifier_resource_bound_is_explicit() {
        assert_eq!(RELAY_ID_BYTES, 16);
    }
}
