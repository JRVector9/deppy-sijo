//! Pure cloud access/control state. The App owns and revokes runtime permits for epochs.

use std::collections::HashMap;

const MAX_ACCESS: usize = 4096;
const MAX_LEASES: usize = 256;
pub(super) const MAX_LEASE_SECONDS: u64 = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Permission {
    pub generation: String,
    pub read: bool,
    pub input: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ControlLease {
    pub connection_id: String,
    pub session_id: String,
    pub generation: String,
    pub run_id: Option<String>,
    pub epoch: u64,
    pub expires_at: u64,
}

pub(super) struct LeaseAcquisition {
    pub lease: ControlLease,
    pub revoked: Vec<u64>,
}

#[derive(Default)]
pub(super) struct ControlManager {
    permissions: HashMap<(String, String), Permission>,
    leases: HashMap<String, ControlLease>,
    next_epoch: u64,
    legacy_connection: Option<String>,
    legacy_revoked: bool,
}

impl ControlManager {
    pub fn with_legacy_connection(id: &str) -> Self {
        Self {
            legacy_connection: Some(id.to_owned()),
            ..Self::default()
        }
    }

    pub fn permission(&self, connection: &str, session: &str, generation: &str) -> Permission {
        self.permissions
            .get(&(connection.to_owned(), session.to_owned()))
            .filter(|permission| permission.generation == generation)
            .cloned()
            .unwrap_or_else(|| Permission {
                generation: generation.to_owned(),
                read: !self.legacy_revoked
                    && self.legacy_connection.as_deref() == Some(connection)
                    && !self
                        .permissions
                        .contains_key(&(connection.to_owned(), session.to_owned())),
                input: !self.legacy_revoked
                    && self.legacy_connection.as_deref() == Some(connection)
                    && !self
                        .permissions
                        .contains_key(&(connection.to_owned(), session.to_owned())),
            })
    }

    pub fn allows(&self, connection: &str, session: &str, generation: &str, input: bool) -> bool {
        let permission = self.permission(connection, session, generation);
        permission.read && (!input || permission.input)
    }

    pub fn set_access(
        &mut self,
        connection: &str,
        session: &str,
        generation: &str,
        read: bool,
        input: bool,
    ) -> Result<Vec<u64>, String> {
        if !valid_identity(connection, 128)
            || !valid_identity(session, 128)
            || !valid_identity(generation, 192)
            || input && !read
        {
            return Err("invalid_client_permission".into());
        }
        let key = (connection.to_owned(), session.to_owned());
        if !self.permissions.contains_key(&key) && self.permissions.len() >= MAX_ACCESS {
            return Err("client_permission_limit".into());
        }
        let next = Permission {
            generation: generation.to_owned(),
            read,
            input,
        };
        let changed = self.permissions.get(&key) != Some(&next);
        self.permissions.insert(key, next);
        Ok(
            if changed
                && self
                    .leases
                    .get(session)
                    .is_some_and(|lease| lease.connection_id == connection)
            {
                self.revoke_session(session)
            } else {
                Vec::new()
            },
        )
    }

    pub fn acquire(
        &mut self,
        connection: &str,
        session: &str,
        generation: &str,
        run: Option<&str>,
        now: u64,
        ttl: u64,
    ) -> Result<LeaseAcquisition, String> {
        if !self.allows(connection, session, generation, true) {
            return Err("client_input_not_allowed".into());
        }
        if ttl == 0 || ttl > MAX_LEASE_SECONDS || run.is_some_and(|id| !valid_identity(id, 128)) {
            return Err("invalid_control_lease".into());
        }
        let expires_at = now.checked_add(ttl).ok_or("invalid_control_lease")?;
        let expired = self
            .leases
            .get(session)
            .is_some_and(|lease| now >= lease.expires_at || lease.generation != generation);
        let mut revoked = if expired {
            self.revoke_session(session)
        } else {
            Vec::new()
        };
        if let Some(lease) = self.leases.get_mut(session) {
            if lease.connection_id != connection || lease.run_id.as_deref() != run {
                return Err("session_control_busy".into());
            }
            if expires_at >= lease.expires_at {
                lease.expires_at = expires_at;
                return Ok(LeaseAcquisition {
                    lease: lease.clone(),
                    revoked,
                });
            }
            // Queued admissions captured the previous expiry. A shorter grant
            // needs a new epoch so the App retires those already queued permits.
            revoked.extend(self.revoke_session(session));
        }
        if self.leases.len() >= MAX_LEASES {
            return Err("control_lease_limit".into());
        }
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .ok_or("control_epoch_exhausted")?;
        let lease = ControlLease {
            connection_id: connection.to_owned(),
            session_id: session.to_owned(),
            generation: generation.to_owned(),
            run_id: run.map(str::to_owned),
            epoch: self.next_epoch,
            expires_at,
        };
        self.leases.insert(session.to_owned(), lease.clone());
        Ok(LeaseAcquisition { lease, revoked })
    }

    pub fn owns(
        &self,
        connection: &str,
        session: &str,
        generation: &str,
        run: Option<&str>,
        epoch: u64,
        now: u64,
    ) -> bool {
        self.allows(connection, session, generation, true)
            && self.leases.get(session).is_some_and(|lease| {
                lease.connection_id == connection
                    && lease.generation == generation
                    && lease.run_id.as_deref() == run
                    && lease.epoch == epoch
                    && now < lease.expires_at
            })
    }

    pub fn lease(&self, session: &str, now: u64) -> Option<&ControlLease> {
        self.leases
            .get(session)
            .filter(|lease| now < lease.expires_at)
    }

    pub fn release(
        &mut self,
        connection: &str,
        session: &str,
        generation: &str,
        run: Option<&str>,
    ) -> Result<Vec<u64>, String> {
        if self.leases.get(session).is_some_and(|lease| {
            lease.connection_id != connection
                || lease.generation != generation
                || lease.run_id.as_deref() != run
        }) {
            return Err("control_not_owned".into());
        }
        Ok(self.revoke_session(session))
    }

    pub fn revoke_session(&mut self, session: &str) -> Vec<u64> {
        self.leases
            .remove(session)
            .map(|lease| vec![lease.epoch])
            .unwrap_or_default()
    }

    pub fn revoke_connection(&mut self, connection: &str) -> Vec<u64> {
        self.permissions.retain(|(actor, _), _| actor != connection);
        if self.legacy_connection.as_deref() == Some(connection) {
            self.legacy_revoked = true;
        }
        self.remove_leases(|lease| lease.connection_id == connection)
    }

    pub fn expire(&mut self, now: u64) -> Vec<u64> {
        self.remove_leases(|lease| now >= lease.expires_at)
    }

    pub fn take_control(&mut self) -> Vec<u64> {
        self.remove_leases(|_| true)
    }

    pub fn retire_auth_connections(&mut self) -> Vec<u64> {
        // Auth rotation removes all named principals. Keep only the legacy ACLs
        // and its explicit revocation marker so rotation cannot widen consent.
        let legacy = self.legacy_connection.as_deref();
        self.permissions
            .retain(|(connection, _), _| legacy == Some(connection.as_str()));
        self.take_control()
    }

    pub fn retain_targets(&mut self, targets: &[(&str, &str)]) -> Vec<u64> {
        self.permissions.retain(|(_, session), permission| {
            targets.contains(&(session.as_str(), permission.generation.as_str()))
        });
        self.remove_leases(|lease| {
            !targets.contains(&(lease.session_id.as_str(), lease.generation.as_str()))
        })
    }

    fn remove_leases(&mut self, remove: impl Fn(&ControlLease) -> bool) -> Vec<u64> {
        let mut revoked = Vec::new();
        self.leases.retain(|_, lease| {
            if remove(lease) {
                revoked.push(lease.epoch);
                false
            } else {
                true
            }
        });
        revoked.sort_unstable();
        revoked
    }
}

fn valid_identity(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_clients_have_no_implicit_access_and_generation_changes_revoke() {
        let mut control = ControlManager::default();
        assert!(!control.allows("bot-a", "pane", "g1", false));
        control
            .set_access("bot-a", "pane", "g1", true, true)
            .unwrap();
        assert!(control.allows("bot-a", "pane", "g1", true));
        assert!(!control.allows("bot-b", "pane", "g1", false));
        assert!(!control.allows("bot-a", "pane", "g2", false));
        let lease = control
            .acquire("bot-a", "pane", "g1", Some("run-a"), 10, 30)
            .unwrap()
            .lease;
        assert_eq!(
            control
                .set_access("bot-a", "pane", "g2", true, true)
                .unwrap(),
            vec![lease.epoch]
        );
        assert!(!control.owns("bot-a", "pane", "g1", Some("run-a"), lease.epoch, 11));
    }

    #[test]
    fn one_writer_per_session_other_sessions_remain_independent() {
        let mut control = ControlManager::default();
        for actor in ["bot-a", "bot-b"] {
            for pane in ["left", "right"] {
                control.set_access(actor, pane, "g", true, true).unwrap();
            }
        }
        let first = control
            .acquire("bot-a", "left", "g", Some("run-a"), 10, 30)
            .unwrap()
            .lease;
        assert!(
            control
                .acquire("bot-b", "left", "g", Some("run-b"), 11, 30)
                .is_err()
        );
        assert!(
            control
                .acquire("bot-b", "right", "g", Some("run-b"), 11, 30)
                .is_ok()
        );
        assert!(
            control
                .release("bot-b", "left", "g", Some("run-b"))
                .is_err()
        );
        assert_eq!(control.revoke_connection("bot-a"), vec![first.epoch]);
        assert!(
            control
                .acquire("bot-b", "left", "g", Some("run-b"), 12, 30)
                .is_ok()
        );
    }

    #[test]
    fn revoked_or_expired_lease_never_reappears_after_reenable() {
        let mut control = ControlManager::default();
        control.set_access("a", "pane", "g", true, true).unwrap();
        let first = control
            .acquire("a", "pane", "g", None, 10, 5)
            .unwrap()
            .lease;
        assert_eq!(control.expire(15), vec![first.epoch]);
        let second = control
            .acquire("a", "pane", "g", None, 15, 5)
            .unwrap()
            .lease;
        assert_ne!(first.epoch, second.epoch);
        assert!(!control.owns("a", "pane", "g", None, first.epoch, 16));
        assert_eq!(
            control.set_access("a", "pane", "g", true, false).unwrap(),
            vec![second.epoch]
        );
        control.set_access("a", "pane", "g", true, true).unwrap();
        assert!(!control.owns("a", "pane", "g", None, second.epoch, 16));
    }

    #[test]
    fn shortening_a_control_lease_retires_queued_admission_epoch() {
        let mut control = ControlManager::default();
        control.set_access("a", "pane", "g", true, true).unwrap();
        let first = control
            .acquire("a", "pane", "g", None, 10, 120)
            .unwrap()
            .lease;
        let second = control.acquire("a", "pane", "g", None, 11, 1).unwrap();
        assert_eq!(second.revoked, vec![first.epoch]);
        assert_ne!(second.lease.epoch, first.epoch);
    }

    #[test]
    fn same_actor_different_run_needs_handoff_and_read_only_cannot_acquire() {
        let mut control = ControlManager::default();
        control.set_access("a", "pane", "g", true, true).unwrap();
        control
            .acquire("a", "pane", "g", Some("one"), 10, 30)
            .unwrap();
        assert!(
            control
                .acquire("a", "pane", "g", Some("two"), 11, 30)
                .is_err()
        );
        control.release("a", "pane", "g", Some("one")).unwrap();
        control
            .acquire("a", "pane", "g", Some("two"), 11, 30)
            .unwrap();
        control
            .set_access("a", "readonly", "g", true, false)
            .unwrap();
        assert!(control.acquire("a", "readonly", "g", None, 11, 30).is_err());
    }

    #[test]
    fn auth_rotation_retires_named_acl_and_epochs_without_widening_legacy_access() {
        let mut control = ControlManager::with_legacy_connection("legacy");
        control
            .set_access("legacy", "allowed", "g", true, false)
            .unwrap();
        control
            .set_access("legacy", "denied", "g", false, false)
            .unwrap();
        control
            .set_access("old-bot", "allowed", "g", true, true)
            .unwrap();
        let first = control
            .acquire("old-bot", "allowed", "g", None, 10, 120)
            .unwrap()
            .lease;
        assert_eq!(control.retire_auth_connections(), vec![first.epoch]);
        assert!(control.allows("legacy", "allowed", "g", false));
        assert!(!control.allows("legacy", "allowed", "g", true));
        assert!(!control.allows("legacy", "denied", "g", false));
        assert!(!control.allows("old-bot", "allowed", "g", false));
        control
            .set_access("new-bot", "allowed", "g", true, true)
            .unwrap();
        let second = control
            .acquire("new-bot", "allowed", "g", None, 11, 120)
            .unwrap()
            .lease;
        assert_ne!(second.epoch, first.epoch);
        control.revoke_connection("legacy");
        control.retire_auth_connections();
        assert!(!control.allows("legacy", "never-consented", "g", false));
    }
}
