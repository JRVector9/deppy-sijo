//! Owner consent and expiring single-writer leases, enforced by runtime permits.
use super::{CloudAgent, Target, control};
use agent_mcp::{ConnectionInfo, ConnectionState, Principal, Request, now};

impl CloudAgent {
    pub fn issue_manual_connection(
        &mut self,
        name: &str,
        input_scope: bool,
    ) -> Result<String, String> {
        if self.issued_leases.len() >= 64 {
            return Err("client_permission_limit".into());
        }
        let server = self.server.as_ref().ok_or("connection_not_active")?;
        let (principal, token) = server
            .auth
            .mint_manual_connection(name, input_scope, now())
            .ok_or("invalid_client_permission")?;
        let secret = secret::SecretString::new(token.as_str().to_owned());
        let lease = match self.redaction.acquire_rotating(&secret) {
            Ok(lease) => lease,
            Err(_) => {
                server.auth.revoke_connection(&principal.id);
                return Err("token_redaction_capacity_no_connection".into());
            }
        };
        self.issued_leases.insert(principal.id, lease);
        Ok(token.as_str().to_owned())
    }
    pub(super) fn retire_run_lease(&mut self, run: &agent_mcp::CloudRun) {
        if (run.state.is_terminal() || run.state == agent_mcp::RunState::Unknown)
            && self
                .control
                .lease(&run.session, now())
                .is_some_and(|lease| {
                    lease.connection_id == run.connection_id
                        && lease.generation == run.generation
                        && lease.run_id.as_deref() == Some(run.id.as_str())
                })
        {
            let revoked = self.control.revoke_session(&run.session);
            self.revoke_leases(revoked);
        }
    }
    pub fn connection_infos(&self) -> Vec<ConnectionInfo> {
        self.server
            .as_ref()
            .map(|s| s.auth.connections())
            .unwrap_or_default()
    }
    pub(super) fn connection_permission(&self, id: &str, target: &Target) -> control::Permission {
        let mut permission = self.control.permission(id, &target.id, &target.generation);
        let grant = self
            .grants
            .get(&target.id)
            .filter(|g| g.generation == target.generation);
        permission.read &= grant.is_some();
        permission.input &= grant.is_some_and(|g| g.input && target.live);
        permission.local_completion &= permission.read && target.live;
        permission
    }
    pub(super) fn connection_lease(&self, target: &Target) -> Option<control::ControlLease> {
        self.control
            .lease(&target.id, now())
            .filter(|l| l.generation == target.generation)
            .cloned()
    }
    pub fn set_connection_access(
        &mut self,
        id: &str,
        target: &Target,
        read: bool,
        input: bool,
    ) -> Result<(), String> {
        let principal = self
            .connection_infos()
            .into_iter()
            .find(|info| info.principal.id == id && info.state == ConnectionState::Active)
            .ok_or("connection_not_active")?
            .principal;
        if input && !principal.input_scope {
            return Err("oauth_input_scope_required".into());
        }
        if !self.targets.iter().any(|t| {
            t.id == target.id
                && t.generation == target.generation
                && t.runtime == target.runtime
                && t.session == target.session
                && t.pane == target.pane
        }) {
            return Err("session_changed_or_closed".into());
        }
        let opening_read = read && !self.grants.contains_key(&target.id);
        let opening_input = input && !self.grants.get(&target.id).is_some_and(|g| g.input);
        let legacy_permission = self.connection_permission(agent_mcp::LEGACY_CONNECTION_ID, target);
        let revoked = self
            .control
            .set_access(id, &target.id, &target.generation, read, input)?;
        self.revoke_leases(revoked);
        if id != agent_mcp::LEGACY_CONNECTION_ID && (opening_read || opening_input) {
            // Preserve the legacy actor's effective consent before raising a
            // shared ceiling. Reserve this ACL entry before any global widening.
            let revoked = self.control.set_access(
                agent_mcp::LEGACY_CONNECTION_ID,
                &target.id,
                &target.generation,
                legacy_permission.read,
                legacy_permission.input,
            )?;
            self.revoke_leases(revoked);
        }
        if opening_read {
            self.share(target, true);
        }
        if opening_input {
            self.allow_input(&target.id, true);
        }
        Ok(())
    }
    pub fn revoke_connection(&mut self, id: &str) {
        if let Some(server) = &self.server {
            server.auth.revoke_connection(id);
        }
        let revoked = self.control.revoke_connection(id);
        self.revoke_leases(revoked);
        self.issued_leases.remove(id);
    }

    pub(super) fn set_connection_local_completion(
        &mut self,
        id: &str,
        target: &Target,
        enabled: bool,
    ) -> Result<(), String> {
        if !self.connection_infos().iter().any(|info| {
            info.principal.id == id && info.state == ConnectionState::Active
        }) {
            return Err("connection_not_active".into());
        }
        if !target.live || !self.targets.iter().any(|current| {
            current.live
                && current.id == target.id
                && current.generation == target.generation
                && current.workspace == target.workspace
                && current.runtime == target.runtime
                && current.runtime_token == target.runtime_token
                && current.session == target.session
                && current.pane == target.pane
        }) {
            return Err("session_changed_or_closed".into());
        }
        if enabled && !self.connection_permission(id, target).read {
            return Err("client_read_not_allowed".into());
        }
        self.control.set_local_completion(id, &target.id, &target.generation, enabled)
    }
    pub fn take_session_control(&mut self, target: &Target) {
        self.allow_input(&target.id, false);
    }
    pub(super) fn revoke_leases(&mut self, epochs: Vec<u64>) {
        for epoch in epochs {
            if let Some(permit) = self.lease_permits.remove(&epoch) {
                permit.revoke();
            }
        }
    }
    pub(super) fn authorize_connection(
        &self,
        principal: &Principal,
        target: &Target,
        input: bool,
    ) -> Result<(), String> {
        self.authorize(&target.id, &target.generation, input)?;
        if input && !principal.input_scope {
            return Err("oauth_input_scope_required".into());
        }
        if !self
            .control
            .allows(&principal.id, &target.id, &target.generation, input)
        {
            return Err(if input {
                "client_input_not_allowed"
            } else {
                "client_read_not_allowed"
            }
            .into());
        }
        Ok(())
    }
    pub(super) fn management_request_current(&self, req: &Request, target: &Target) -> bool {
        self.server.as_ref().is_some_and(|s| req.live(&s.auth))
            && self
                .authorize_connection(&req.principal, target, false)
                .is_ok()
            && self.targets.iter().any(|t| {
                t.id == target.id
                    && t.generation == target.generation
                    && t.runtime == target.runtime
                    && t.session == target.session
                    && t.pane == target.pane
                    && t.workspace == target.workspace
            })
    }
    pub(super) fn acquire_lease(
        &mut self,
        req: &Request,
        target: &Target,
        run: Option<&str>,
        ttl: u64,
    ) -> Result<control::ControlLease, String> {
        self.authorize_connection(&req.principal, target, true)?;
        if run.is_some_and(|run| !self.management_run_owned(&req.principal.id, run, target)) {
            return Err("run_not_loaded_or_not_owned".into());
        }
        let acquired = self.control.acquire(
            &req.principal.id,
            &target.id,
            &target.generation,
            run,
            now(),
            ttl,
        )?;
        self.revoke_leases(acquired.revoked);
        self.lease_permits.entry(acquired.lease.epoch).or_default();
        Ok(acquired.lease)
    }
    pub fn save_profile(&mut self) -> Result<(), String> {
        let profile = crate::config::CloudConnectionProfile {
            name: self.profile_name.trim().to_owned(),
            port: self.port,
            public_host: self.hostname.clone(),
            automatic: self.automatic,
        };
        if !profile.valid() {
            return Err("invalid_connection_profile".into());
        }
        if let Some(old) = self.profiles.iter_mut().find(|p| p.name == profile.name) {
            *old = profile;
        } else if self.profiles.len() < 8 {
            self.profiles.push(profile);
        } else {
            return Err("connection_profile_limit".into());
        }
        Ok(())
    }
    pub fn load_profile(&mut self, index: usize) -> Result<(), String> {
        if self.busy() {
            return Err("connection_must_be_stopped".into());
        }
        let profile = self
            .profiles
            .get(index)
            .ok_or("invalid_connection_profile")?;
        self.port = profile.port;
        self.hostname = profile.public_host.clone();
        self.automatic = profile.automatic;
        self.profile_name = profile.name.clone();
        Ok(())
    }
    pub fn remove_profile(&mut self, index: usize) {
        if index < self.profiles.len() {
            self.profiles.remove(index);
        }
    }
}
