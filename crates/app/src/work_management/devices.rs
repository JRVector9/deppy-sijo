//! Bounded compute registrations. A Relay viewer is never an execution grant.
use deppy_core::WorkspaceId;
use deppy_core::work_management::{AgentSelection, AttemptId, OperationId, RuntimeIncarnationId};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
pub use storage::work_management::RegisteredExecutionDevice as RegisteredDevice;

pub const EXECUTION_DEVICE_MAX: usize = 32;
pub const EXECUTION_PROFILE_MAX: usize = 32;
pub const EXECUTION_LEASE_MAX: usize = 256;
pub const EXECUTION_FRESHNESS_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerProfile {
    pub id: String,
    pub agent: AgentSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerObservation {
    pub device_id: String,
    pub pinned_fingerprint: String,
    pub connection_epoch: u64,
    pub incarnation: RuntimeIncarnationId,
    pub workspace_id: WorkspaceId,
    pub folder: String,
    pub folder_anchor: String,
    pub grant_epoch: u64,
    pub lease_epoch: u64,
    pub profiles: Vec<WorkerProfile>,
    pub max_workers: u8,
    pub active_workers: u8,
    pub active_operation_ids: Vec<OperationId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRequest {
    pub device_id: String,
    pub workspace_id: WorkspaceId,
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub folder: String,
    pub agent: AgentSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLease {
    pub request: LeaseRequest,
    pub folder_anchor: String,
    pub profile_id: String,
    pub grant_epoch: u64,
    pub lease_epoch: u64,
    pub connection_epoch: u64,
    pub incarnation: RuntimeIncarnationId,
}

/// Read-only proof minted from an exact trusted connection, never from UI fields.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedExecutionDevice {
    registration: RegisteredDevice,
    profiles: Vec<WorkerProfile>,
    observed_at: std::time::Instant,
    fence: Arc<AtomicBool>,
}
impl VerifiedExecutionDevice {
    pub(crate) fn registration(&self) -> &RegisteredDevice {
        &self.registration
    }
    pub(crate) fn profiles(&self) -> &[WorkerProfile] {
        &self.profiles
    }
    pub(crate) fn validate_fresh(&self) -> Result<(), DeviceUnavailable> {
        (self.fence.load(Ordering::Acquire)
            && self.observed_at.elapsed()
                <= std::time::Duration::from_millis(EXECUTION_FRESHNESS_MS))
        .then_some(())
        .ok_or(DeviceUnavailable::Stale)
    }
}
#[derive(Debug, Clone)]
pub(crate) struct VerifiedExecutionLease(Arc<VerifiedLeaseData>);
#[derive(Debug)]
struct VerifiedLeaseData {
    lease: ExecutionLease,
    registration: RegisteredDevice,
    observed_at: std::time::Instant,
    fence: Arc<AtomicBool>,
}
impl VerifiedExecutionLease {
    pub(crate) fn registration(&self) -> &RegisteredDevice {
        &self.0.registration
    }
    pub(crate) fn lease(&self) -> &ExecutionLease {
        &self.0.lease
    }
    pub(crate) fn validate_fresh(&self) -> Result<(), DeviceUnavailable> {
        (self.0.fence.load(Ordering::Acquire)
            && self.0.observed_at.elapsed()
                <= std::time::Duration::from_millis(EXECUTION_FRESHNESS_MS))
        .then_some(())
        .ok_or(DeviceUnavailable::Stale)
    }
}

impl ExecutionLease {
    pub fn validate(&self) -> Result<(), DeviceUnavailable> {
        if !valid_id(&self.request.device_id)
            || !valid_id(&self.request.workspace_id.0)
            || !valid_id(self.request.attempt_id.as_str())
            || !valid_id(self.request.operation_id.as_str())
            || !valid_folder(&self.request.folder)
            || self.request.agent.validate().is_err()
            || !valid_id(&self.folder_anchor)
            || !valid_id(&self.profile_id)
            || !valid_id(self.incarnation.as_str())
            || self.grant_epoch == 0
            || self.lease_epoch == 0
            || self.connection_epoch == 0
        {
            return Err(DeviceUnavailable::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceUnavailable {
    Invalid,
    Full,
    ViewerOnly,
    Missing,
    Revoked,
    Disconnected,
    Stale,
    IdentityChanged,
    FolderChanged,
    UnsupportedAgent,
    Capacity,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub can_execute: bool,
    pub unavailable: Option<DeviceUnavailable>,
    pub profiles: Vec<WorkerProfile>,
}

#[derive(Clone)]
struct DeviceEntry {
    registration: RegisteredDevice,
    observation: Option<(WorkerObservation, u64)>,
    disconnected_through: u64,
    fence: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct DeviceRegistry {
    entries: Vec<DeviceEntry>,
    leases: Vec<ExecutionLease>,
    viewers: Vec<(String, String, WorkspaceId, bool)>,
}

impl DeviceRegistry {
    pub(crate) fn verified_devices(
        &self,
        workspace: &WorkspaceId,
        now_ms: u64,
    ) -> Vec<VerifiedExecutionDevice> {
        self.entries
            .iter()
            .filter(|e| e.registration.workspace_id == *workspace)
            .filter_map(|e| {
                let proof = fresh_observation(e, now_ms).ok()?;
                let age = now_ms.checked_sub(e.observation.as_ref()?.1)?;
                Some(VerifiedExecutionDevice {
                    registration: e.registration.clone(),
                    fence: Arc::clone(&e.fence),
                    profiles: proof
                        .profiles
                        .iter()
                        .filter(|p| e.registration.allowed_agents.contains(&p.agent))
                        .cloned()
                        .collect(),
                    observed_at: std::time::Instant::now()
                        .checked_sub(std::time::Duration::from_millis(age))?,
                })
            })
            .collect()
    }
    pub(crate) fn verified_lease(
        &self,
        lease: &ExecutionLease,
        now_ms: u64,
    ) -> Result<VerifiedExecutionLease, DeviceUnavailable> {
        self.validate_lease(lease, now_ms)?;
        self.mint_verified_lease(lease, now_ms)
    }
    pub(crate) fn verified_execution_lease(
        &self,
        lease: &ExecutionLease,
        now_ms: u64,
    ) -> Result<VerifiedExecutionLease, DeviceUnavailable> {
        self.validate_execution_lease(lease, now_ms)?;
        self.mint_verified_lease(lease, now_ms)
    }
    fn mint_verified_lease(
        &self,
        lease: &ExecutionLease,
        now_ms: u64,
    ) -> Result<VerifiedExecutionLease, DeviceUnavailable> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.registration.id == lease.request.device_id)
            .ok_or(DeviceUnavailable::Missing)?;
        let (_, observed) = entry
            .observation
            .as_ref()
            .ok_or(DeviceUnavailable::Missing)?;
        let age = now_ms
            .checked_sub(*observed)
            .ok_or(DeviceUnavailable::Stale)?;
        Ok(VerifiedExecutionLease(Arc::new(VerifiedLeaseData {
            lease: lease.clone(),
            registration: entry.registration.clone(),
            fence: Arc::clone(&entry.fence),
            observed_at: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_millis(age))
                .ok_or(DeviceUnavailable::Stale)?,
        })))
    }
    pub fn revoke(&mut self, id: &str) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.registration.id == id) {
            entry.fence.store(false, Ordering::Release);
            entry.registration.execution_granted = false;
            entry.observation = None;
            true
        } else {
            false
        }
    }

    pub fn disconnect(&mut self, device: &str, connection_epoch: u64) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.registration.id == device)
        {
            entry.disconnected_through = entry.disconnected_through.max(connection_epoch);
            if entry
                .observation
                .as_ref()
                .is_some_and(|(p, _)| p.connection_epoch <= connection_epoch)
            {
                entry.fence.store(false, Ordering::Release);
                entry.observation = None;
            }
        }
    }
    pub(crate) fn invalidate_all(&self) {
        for entry in &self.entries {
            entry.fence.store(false, Ordering::Release);
        }
    }
    /// Must be checked immediately before a first send. An idempotent reserve is not replay permission.
    pub fn validate_lease(
        &self,
        lease: &ExecutionLease,
        now_ms: u64,
    ) -> Result<(), DeviceUnavailable> {
        if !self.leases.contains(lease) {
            return Err(DeviceUnavailable::Missing);
        }
        let entry = self
            .entries
            .iter()
            .find(|e| e.registration.id == lease.request.device_id)
            .ok_or(DeviceUnavailable::Missing)?;
        let proof = fresh_observation(entry, now_ms)?;
        if proof.connection_epoch != lease.connection_epoch
            || proof.incarnation != lease.incarnation
            || proof.grant_epoch != lease.grant_epoch
            || proof.lease_epoch != lease.lease_epoch
            || proof.folder_anchor != lease.folder_anchor
            || !proof
                .profiles
                .iter()
                .any(|p| p.id == lease.profile_id && p.agent == lease.request.agent)
        {
            return Err(DeviceUnavailable::Conflict);
        }
        Ok(())
    }
    /// Ongoing exact execution bindings survive a newer capacity lease observation, while
    /// current connection, incarnation, grant, folder and profile remain mandatory.
    pub(crate) fn validate_execution_lease(
        &self,
        lease: &ExecutionLease,
        now_ms: u64,
    ) -> Result<(), DeviceUnavailable> {
        if !self.leases.contains(lease) {
            return Err(DeviceUnavailable::Missing);
        }
        let entry = self
            .entries
            .iter()
            .find(|e| e.registration.id == lease.request.device_id)
            .ok_or(DeviceUnavailable::Missing)?;
        let proof = fresh_observation(entry, now_ms)?;
        if proof.connection_epoch != lease.connection_epoch
            || proof.incarnation != lease.incarnation
            || proof.grant_epoch != lease.grant_epoch
            || proof.folder_anchor != lease.folder_anchor
            || entry.registration.folder != lease.request.folder
            || !proof
                .profiles
                .iter()
                .any(|p| p.id == lease.profile_id && p.agent == lease.request.agent)
        {
            return Err(DeviceUnavailable::Conflict);
        }
        Ok(())
    }
    /// Call only after an exact operation completion/rejection receipt, never on timeout or disconnect.
    pub fn complete(&mut self, lease: &ExecutionLease) -> bool {
        if let Some(index) = self.leases.iter().position(|l| l == lease) {
            self.leases.remove(index);
            true
        } else {
            false
        }
    }
    pub fn snapshot(&self, workspace: &WorkspaceId, now_ms: u64) -> Vec<DeviceView> {
        let mut views: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.registration.workspace_id == *workspace)
            .map(|e| {
                let proof = fresh_observation(e, now_ms);
                let unavailable = proof.as_ref().err().copied().or_else(|| {
                    let p = proof.as_ref().ok()?;
                    let reserved = self
                        .leases
                        .iter()
                        .filter(|l| {
                            l.request.device_id == e.registration.id
                                && !p.active_operation_ids.contains(&l.request.operation_id)
                        })
                        .count();
                    (reserved + usize::from(p.active_workers)
                        >= usize::from(e.registration.max_workers.min(p.max_workers)))
                    .then_some(DeviceUnavailable::Capacity)
                });
                DeviceView {
                    id: e.registration.id.clone(),
                    name: e.registration.name.clone(),
                    connected: e.observation.is_some(),
                    can_execute: unavailable.is_none(),
                    unavailable,
                    profiles: proof
                        .ok()
                        .map(|p| {
                            p.profiles
                                .iter()
                                .filter(|p| e.registration.allowed_agents.contains(&p.agent))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect();
        views.extend(
            self.viewers
                .iter()
                .filter(|v| v.2 == *workspace)
                .map(|v| DeviceView {
                    id: v.0.clone(),
                    name: v.1.clone(),
                    connected: v.3,
                    can_execute: false,
                    unavailable: Some(DeviceUnavailable::ViewerOnly),
                    profiles: vec![],
                }),
        );
        views
    }
    pub(crate) fn check_registration_update(
        &self,
        device: &RegisteredDevice,
    ) -> Result<(), DeviceUnavailable> {
        validate_registration(device)?;
        if self.viewers.iter().any(|v| v.0 == device.id) {
            return Err(DeviceUnavailable::ViewerOnly);
        }
        if let Some(entry) = self.entries.iter().find(|e| e.registration.id == device.id) {
            if entry.registration != *device && device.grant_epoch <= entry.registration.grant_epoch
            {
                return Err(DeviceUnavailable::Conflict);
            }
        } else if self.entries.len() + self.viewers.len() >= EXECUTION_DEVICE_MAX {
            return Err(DeviceUnavailable::Full);
        }
        Ok(())
    }
    pub fn register(&mut self, device: RegisteredDevice) -> Result<(), DeviceUnavailable> {
        self.check_registration_update(&device)?;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.registration.id == device.id)
        {
            if entry.registration == device {
                return Ok(());
            }
            if device.grant_epoch <= entry.registration.grant_epoch {
                return Err(DeviceUnavailable::Conflict);
            }
            entry.fence.store(false, Ordering::Release);
            entry.registration = device;
            entry.observation = None;
            return Ok(());
        }
        if self.entries.len() + self.viewers.len() >= EXECUTION_DEVICE_MAX {
            return Err(DeviceUnavailable::Full);
        }
        self.entries.push(DeviceEntry {
            registration: device,
            observation: None,
            disconnected_through: 0,
            fence: Arc::new(AtomicBool::new(false)),
        });
        Ok(())
    }

    pub fn observe(
        &mut self,
        proof: WorkerObservation,
        now_ms: u64,
    ) -> Result<(), DeviceUnavailable> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.registration.id == proof.device_id)
            .ok_or(DeviceUnavailable::Missing)?;
        if proof.connection_epoch <= entry.disconnected_through {
            return Err(DeviceUnavailable::Conflict);
        }
        let device = &entry.registration;
        if proof.pinned_fingerprint != device.pinned_fingerprint {
            return Err(DeviceUnavailable::IdentityChanged);
        }
        if proof.workspace_id != device.workspace_id
            || proof.folder != device.folder
            || proof.folder_anchor != device.folder_anchor
        {
            return Err(DeviceUnavailable::FolderChanged);
        }
        if !device.execution_granted || proof.grant_epoch != device.grant_epoch {
            return Err(DeviceUnavailable::Revoked);
        }
        if proof.connection_epoch == 0
            || proof.lease_epoch == 0
            || !valid_id(proof.incarnation.as_str())
            || proof.max_workers == 0
            || proof.max_workers > 8
            || proof.active_workers > proof.max_workers
            || proof.active_operation_ids.len() > usize::from(proof.active_workers)
            || proof
                .active_operation_ids
                .iter()
                .enumerate()
                .any(|(i, id)| {
                    !valid_id(id.as_str()) || proof.active_operation_ids[..i].contains(id)
                })
            || proof.profiles.is_empty()
            || proof.profiles.len() > EXECUTION_PROFILE_MAX
        {
            return Err(DeviceUnavailable::Invalid);
        }
        for (i, profile) in proof.profiles.iter().enumerate() {
            if !valid_id(&profile.id)
                || profile.agent.validate().is_err()
                || proof.profiles[..i].iter().any(|p| p.id == profile.id)
            {
                return Err(DeviceUnavailable::Invalid);
            }
        }
        if let Some((previous, observed_at)) = &entry.observation
            && (proof.connection_epoch < previous.connection_epoch
                || now_ms < *observed_at
                || (proof.connection_epoch == previous.connection_epoch
                    && (proof.incarnation != previous.incarnation
                        || proof.lease_epoch < previous.lease_epoch)))
        {
            return Err(DeviceUnavailable::Conflict);
        }
        let same_identity = entry.observation.as_ref().is_some_and(|(previous, _)| {
            previous.device_id == proof.device_id
                && previous.pinned_fingerprint == proof.pinned_fingerprint
                && previous.connection_epoch == proof.connection_epoch
                && previous.incarnation == proof.incarnation
                && previous.workspace_id == proof.workspace_id
                && previous.folder == proof.folder
                && previous.folder_anchor == proof.folder_anchor
                && previous.grant_epoch == proof.grant_epoch
                && previous.profiles == proof.profiles
                && previous.max_workers == proof.max_workers
        });
        if !same_identity {
            entry.fence.store(false, Ordering::Release);
            entry.fence = Arc::new(AtomicBool::new(true));
        }
        entry.observation = Some((proof, now_ms));
        Ok(())
    }

    pub fn reserve(
        &mut self,
        request: LeaseRequest,
        now_ms: u64,
    ) -> Result<ExecutionLease, DeviceUnavailable> {
        if let Some(existing) = self.leases.iter().find(|l| {
            l.request.operation_id == request.operation_id
                || l.request.attempt_id == request.attempt_id
        }) {
            return if existing.request == request {
                Ok(existing.clone())
            } else {
                Err(DeviceUnavailable::Conflict)
            };
        }
        if self.viewers.iter().any(|v| v.0 == request.device_id) {
            return Err(DeviceUnavailable::ViewerOnly);
        }
        if !valid_id(request.attempt_id.as_str())
            || !valid_id(request.operation_id.as_str())
            || request.agent.validate().is_err()
        {
            return Err(DeviceUnavailable::Invalid);
        }
        let entry = self
            .entries
            .iter()
            .find(|e| e.registration.id == request.device_id)
            .ok_or(DeviceUnavailable::Missing)?;
        let proof = fresh_observation(entry, now_ms)?;
        if request.workspace_id != entry.registration.workspace_id
            || request.folder != entry.registration.folder
        {
            return Err(DeviceUnavailable::FolderChanged);
        }
        if !entry.registration.allowed_agents.contains(&request.agent) {
            return Err(DeviceUnavailable::UnsupportedAgent);
        }
        let mut matching = proof.profiles.iter().filter(|p| p.agent == request.agent);
        let profile = matching.next().ok_or(DeviceUnavailable::UnsupportedAgent)?;
        if matching.next().is_some() {
            return Err(DeviceUnavailable::UnsupportedAgent);
        }
        let reserved = self
            .leases
            .iter()
            .filter(|l| {
                l.request.device_id == request.device_id
                    && !proof.active_operation_ids.contains(&l.request.operation_id)
            })
            .count();
        // A reservation remains occupied through uncertain delivery. Never reclaim on timeout.
        if reserved + usize::from(proof.active_workers)
            >= usize::from(entry.registration.max_workers.min(proof.max_workers))
        {
            return Err(DeviceUnavailable::Capacity);
        }
        if self.leases.len() == EXECUTION_LEASE_MAX {
            return Err(DeviceUnavailable::Full);
        }
        let lease = ExecutionLease {
            request,
            folder_anchor: proof.folder_anchor.clone(),
            profile_id: profile.id.clone(),
            grant_epoch: proof.grant_epoch,
            lease_epoch: proof.lease_epoch,
            connection_epoch: proof.connection_epoch,
            incarnation: proof.incarnation.clone(),
        };
        self.leases.push(lease.clone());
        Ok(lease)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn valid_folder(value: &str) -> bool {
    value.len() <= 8192
        && std::path::Path::new(value).is_absolute()
        && !value.chars().any(char::is_control)
        && !std::path::Path::new(value)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

fn validate_registration(device: &RegisteredDevice) -> Result<(), DeviceUnavailable> {
    if device.validate().is_err() {
        return Err(DeviceUnavailable::Invalid);
    }
    let fingerprint = device.pinned_fingerprint.as_bytes();
    let valid_pin = fingerprint.len() == 95
        && fingerprint.iter().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                *b == b':'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        });
    if !valid_id(&device.id)
        || device.name.is_empty()
        || device.name.len() > 256
        || device.name.chars().any(char::is_control)
        || !valid_pin
        || device.workspace_id.0.is_empty()
        || device.workspace_id.0.len() > 128
        || !valid_folder(&device.folder)
        || !valid_id(&device.folder_anchor)
        || device.allowed_agents.is_empty()
        || device.allowed_agents.len() > EXECUTION_PROFILE_MAX
        || device.allowed_agents.iter().any(|a| a.validate().is_err())
        || !(1..=8).contains(&device.max_workers)
        || device.grant_epoch == 0
    {
        return Err(DeviceUnavailable::Invalid);
    }
    Ok(())
}

fn fresh_observation(
    entry: &DeviceEntry,
    now_ms: u64,
) -> Result<&WorkerObservation, DeviceUnavailable> {
    if !entry.registration.execution_granted {
        return Err(DeviceUnavailable::Revoked);
    }
    let (proof, observed_at) = entry
        .observation
        .as_ref()
        .ok_or(DeviceUnavailable::Disconnected)?;
    if now_ms
        .checked_sub(*observed_at)
        .is_none_or(|age| age > EXECUTION_FRESHNESS_MS)
    {
        return Err(DeviceUnavailable::Stale);
    }
    Ok(proof)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn registration() -> RegisteredDevice {
        RegisteredDevice {
            id: "worker-a".into(),
            name: "Worker A".into(),
            endpoint: "127.0.0.1:9000".parse().unwrap(),
            pinned_fingerprint: vec!["aa"; 32].join(":"),
            workspace_id: WorkspaceId("workspace-a".into()),
            folder: "/fixture/a".into(),
            folder_anchor: "anchor-a".into(),
            allowed_agents: vec![AgentSelection {
                provider: "codex".into(),
                model: Some("gpt-6.1-sol".into()),
                effort: Some("xhigh".into()),
            }],
            max_workers: 1,
            grant_epoch: 1,
            execution_granted: true,
        }
    }
    fn proof(device: &RegisteredDevice) -> WorkerObservation {
        WorkerObservation {
            device_id: device.id.clone(),
            pinned_fingerprint: device.pinned_fingerprint.clone(),
            connection_epoch: 1,
            incarnation: RuntimeIncarnationId("incarnation-a".into()),
            workspace_id: device.workspace_id.clone(),
            folder: device.folder.clone(),
            folder_anchor: device.folder_anchor.clone(),
            grant_epoch: device.grant_epoch,
            lease_epoch: 1,
            profiles: vec![WorkerProfile {
                id: "profile-a".into(),
                agent: device.allowed_agents[0].clone(),
            }],
            max_workers: 1,
            active_workers: 0,
            active_operation_ids: vec![],
        }
    }
    fn request(device: &RegisteredDevice) -> LeaseRequest {
        LeaseRequest {
            device_id: device.id.clone(),
            workspace_id: device.workspace_id.clone(),
            attempt_id: AttemptId("attempt-a".into()),
            operation_id: OperationId("operation-a".into()),
            folder: device.folder.clone(),
            agent: device.allowed_agents[0].clone(),
        }
    }
    #[test]
    fn speculative_registration_validation_never_retires_the_live_grant() {
        let mut registry = DeviceRegistry::default();
        let device = registration();
        registry.register(device.clone()).unwrap();
        registry.observe(proof(&device), 10).unwrap();
        let live = registry
            .verified_devices(&device.workspace_id, 10)
            .remove(0);
        let mut proposed = device.clone();
        proposed.grant_epoch += 1;
        registry.check_registration_update(&proposed).unwrap();
        assert!(
            live.validate_fresh().is_ok(),
            "uncommitted replacement cannot revoke the existing live grant"
        );
        assert!(
            registry
                .verified_devices(&device.workspace_id, 11)
                .remove(0)
                .validate_fresh()
                .is_ok()
        );
        registry.register(proposed).unwrap();
        assert!(
            live.validate_fresh().is_err(),
            "only committed replacement retires old proofs"
        );
    }
    #[test]
    fn same_agent_profile_aliases_never_choose_an_unapproved_profile() {
        let mut registry = DeviceRegistry::default();
        let device = registration();
        registry.register(device.clone()).unwrap();
        let mut observed = proof(&device);
        observed.profiles.push(WorkerProfile {
            id: "another-profile".into(),
            agent: device.allowed_agents[0].clone(),
        });
        registry.observe(observed, 10).unwrap();
        assert_eq!(
            registry.reserve(request(&device), 10),
            Err(DeviceUnavailable::UnsupportedAgent)
        );
    }
    #[test]
    fn verified_device_and_lease_clones_are_revoked_by_live_registry_changes() {
        let mut registry = DeviceRegistry::default();
        let device = registration();
        registry.register(device.clone()).unwrap();
        registry.observe(proof(&device), 10).unwrap();
        let policy = registry
            .verified_devices(&device.workspace_id, 10)
            .remove(0);
        assert_eq!(policy.registration(), &device);
        assert_eq!(policy.profiles()[0].agent, device.allowed_agents[0]);
        let lease = registry.reserve(request(&device), 10).unwrap();
        let verified = registry.verified_lease(&lease, 10).unwrap();
        assert_eq!(verified.lease(), &lease);
        let duplicate = verified.clone();
        assert!(policy.validate_fresh().is_ok());
        assert!(verified.validate_fresh().is_ok());
        let mut heartbeat = proof(&device);
        heartbeat.active_workers = 1;
        heartbeat.active_operation_ids = vec![lease.request.operation_id.clone()];
        heartbeat.lease_epoch += 1;
        registry.observe(heartbeat, 11).unwrap();
        assert_eq!(
            registry
                .verified_execution_lease(&lease, 11)
                .unwrap()
                .lease(),
            &lease
        );
        assert!(
            policy.validate_fresh().is_ok(),
            "capacity heartbeat cannot retire an unchanged identity"
        );
        assert!(duplicate.validate_fresh().is_ok());
        registry.disconnect(&device.id, 1);
        assert!(policy.validate_fresh().is_err());
        assert!(duplicate.validate_fresh().is_err());
        let mut observed = proof(&device);
        observed.connection_epoch = 2;
        registry.observe(observed, 20).unwrap();
        let policy = registry
            .verified_devices(&device.workspace_id, 20)
            .remove(0);
        registry.disconnect(&device.id, 1);
        assert!(
            policy.validate_fresh().is_ok(),
            "stale close cannot revoke a newer connection"
        );
        registry.revoke(&device.id);
        assert!(policy.validate_fresh().is_err());
    }
    #[test]
    fn registered_compute_requires_exact_fresh_proof_and_reserves_capacity() {
        let mut registry = DeviceRegistry::default();
        let device = registration();
        registry.register(device.clone()).unwrap();
        assert_eq!(
            registry.reserve(request(&device), 10),
            Err(DeviceUnavailable::Disconnected)
        );
        registry.observe(proof(&device), 10).unwrap();
        let first = registry.reserve(request(&device), 11).unwrap();
        assert_eq!(registry.reserve(request(&device), 12).unwrap(), first);
        let mut other = request(&device);
        other.attempt_id = AttemptId("attempt-b".into());
        other.operation_id = OperationId("operation-b".into());
        assert_eq!(
            registry.reserve(other, 12),
            Err(DeviceUnavailable::Capacity)
        );
        let mut wrong = request(&device);
        wrong.folder = "/fixture/b".into();
        assert_eq!(
            registry.reserve(wrong, 12),
            Err(DeviceUnavailable::Conflict)
        );
    }
    #[test]
    fn stale_pin_folder_and_grant_proofs_never_enable_execution() {
        let mut registry = DeviceRegistry::default();
        let device = registration();
        registry.register(device.clone()).unwrap();
        let mut wrong = proof(&device);
        wrong.pinned_fingerprint = vec!["bb"; 32].join(":");
        assert_eq!(
            registry.observe(wrong, 10),
            Err(DeviceUnavailable::IdentityChanged)
        );
        wrong = proof(&device);
        wrong.folder_anchor = "moved-folder".into();
        assert_eq!(
            registry.observe(wrong, 10),
            Err(DeviceUnavailable::FolderChanged)
        );
        registry.observe(proof(&device), 10).unwrap();
        assert_eq!(
            registry.reserve(request(&device), 10 + EXECUTION_FRESHNESS_MS + 1),
            Err(DeviceUnavailable::Stale)
        );
    }

    #[test]
    fn malformed_registrations_and_cross_worker_aliases_are_rejected() {
        let mut registry = DeviceRegistry::default();
        let mut device = registration();
        device.folder = "/fixture/../escape".into();
        assert_eq!(registry.register(device), Err(DeviceUnavailable::Invalid));
        let a = registration();
        registry.register(a.clone()).unwrap();
        registry.observe(proof(&a), 10).unwrap();
        let lease = registry.reserve(request(&a), 10).unwrap();
        let mut b = a.clone();
        b.id = "worker-b".into();
        b.folder = "/fixture/b".into();
        b.folder_anchor = "anchor-b".into();
        registry.register(b.clone()).unwrap();
        registry.observe(proof(&b), 10).unwrap();
        let mut alias = request(&b);
        assert_eq!(
            registry.reserve(alias.clone(), 10),
            Err(DeviceUnavailable::Conflict)
        );
        alias.operation_id = OperationId("operation-b".into());
        alias.attempt_id = AttemptId("attempt-b".into());
        assert_eq!(
            registry.reserve(alias, 10).unwrap().request.device_id,
            "worker-b"
        );
        assert!(registry.complete(&lease));
    }
    #[test]
    fn admitted_local_leases_are_not_double_counted_but_unrelated_work_is() {
        let mut device = registration();
        device.max_workers = 2;
        let mut registry = DeviceRegistry::default();
        registry.register(device.clone()).unwrap();
        let mut observed = proof(&device);
        observed.max_workers = 2;
        registry.observe(observed.clone(), 10).unwrap();
        let first = registry.reserve(request(&device), 10).unwrap();
        observed.active_workers = 1;
        observed.active_operation_ids = vec![first.request.operation_id.clone()];
        registry.observe(observed.clone(), 11).unwrap();
        let mut second = request(&device);
        second.operation_id = OperationId("operation-b".into());
        second.attempt_id = AttemptId("attempt-b".into());
        registry.reserve(second, 11).unwrap();
        assert!(!registry.snapshot(&device.workspace_id, 11)[0].can_execute);
        registry.complete(&first);
        observed.active_operation_ids.clear();
        registry.observe(observed, 12).unwrap();
        let mut third = request(&device);
        third.operation_id = OperationId("operation-c".into());
        third.attempt_id = AttemptId("attempt-c".into());
        assert_eq!(
            registry.reserve(third, 12),
            Err(DeviceUnavailable::Capacity)
        );
    }
}
