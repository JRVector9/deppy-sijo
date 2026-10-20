//! Lazy worker boundary: TLS, keychain, SQLite, waits, and client destruction stay off-frame.
use super::*;
type Result<T> = std::result::Result<T, RemoteDeviceError>;
struct Connection {
    device_id: String,
    workspace: WorkspaceId,
    client: RemoteRuntimeClient,
    events: RuntimeEventReceiver,
    epoch: u64,
    pin: String,
    last_observed: Instant,
    pending_inspect: Option<(u64, Instant)>,
    deferred: Vec<RuntimeEvent>,
}
struct Candidate {
    view: RemoteDeviceCandidate,
    connection: Connection,
    token: SecretString,
    created: Instant,
}
struct Flight {
    request_id: u64,
    lease: ExecutionLease,
    request: RemoteWorkRequest,
    result: Option<RemoteWorkResult>,
    pending_result: Option<RemoteWorkResult>,
    execution: Option<RemoteExecutionProof>,
}
struct InputFlight {
    request_id: u64,
    lease: ExecutionLease,
    request: RemoteTaskInputRequest,
    operation_id: OperationId,
    result: Option<RemoteTaskInputResult>,
    accepted: Option<RemoteInputAcceptance>,
    emitted: bool,
    started: Instant,
}
pub(super) struct ControllerState {
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
    store: Option<CommandCenterStore>,
    registry: DeviceRegistry,
    records: Vec<StoredExecutionDevice>,
    loaded: Vec<WorkspaceId>,
    connections: Vec<Connection>,
    candidates: Vec<Candidate>,
    flights: Vec<Flight>,
    inputs: Vec<InputFlight>,
    epoch: u64,
    query: u64,
    start: Instant,
    poll_cursor: usize,
}
impl Drop for ControllerState {
    fn drop(&mut self) {
        self.registry.invalidate_all();
    }
}
impl ControllerState {
    pub(super) fn new(data_dir: PathBuf, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            data_dir,
            secrets,
            store: None,
            registry: DeviceRegistry::default(),
            records: vec![],
            loaded: vec![],
            connections: vec![],
            candidates: vec![],
            flights: vec![],
            inputs: vec![],
            epoch: 0,
            query: 1,
            start: Instant::now(),
            poll_cursor: 0,
        }
    }
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn store(&mut self) -> Result<&mut CommandCenterStore> {
        if self.store.is_none() {
            self.store = Some(
                CommandCenterStore::open(&self.data_dir)
                    .map_err(|_| RemoteDeviceError::Persistence)?,
            );
        }
        self.store.as_mut().ok_or(RemoteDeviceError::Persistence)
    }
    fn load(&mut self, workspace: &WorkspaceId) -> Result<()> {
        if self.loaded.contains(workspace) {
            return Ok(());
        }
        let records = self
            .store()?
            .load_execution_devices(workspace)
            .map_err(|_| RemoteDeviceError::Persistence)?;
        if self.records.len() + records.len() > 32 {
            return Err(RemoteDeviceError::Full);
        }
        for record in records {
            self.registry
                .register(record.registration.clone())
                .map_err(|_| RemoteDeviceError::Invalid)?;
            self.records.push(record);
        }
        self.loaded.push(workspace.clone());
        Ok(())
    }
    pub(super) fn execute(&mut self, job: RemoteDeviceJob) -> RemoteDeviceOutcome {
        let request_id = match &job {
            RemoteDeviceJob::Inspect { setup } => setup.request_id,
            RemoteDeviceJob::Grant { request_id, .. }
            | RemoteDeviceJob::Revoke { request_id, .. }
            | RemoteDeviceJob::ReserveLaunch { request_id, .. }
            | RemoteDeviceJob::ObserveExecution { request_id, .. }
            | RemoteDeviceJob::DeliverInput { request_id, .. } => *request_id,
            _ => 0,
        };
        self.candidates
            .retain(|c| c.created.elapsed() < Duration::from_secs(300));
        let result = match job {
            RemoteDeviceJob::Inspect { setup } => self.inspect(setup),
            RemoteDeviceJob::Grant {
                request_id,
                candidate_id,
                allowed_profile_ids,
                max_workers,
            } => self.grant(request_id, &candidate_id, allowed_profile_ids, max_workers),
            RemoteDeviceJob::Load { workspace_id } => {
                self.load(&workspace_id)
                    .map(|()| RemoteDeviceOutcome::Loaded {
                        devices: self.registry.snapshot(&workspace_id, self.now()),
                        workspace_id,
                    })
            }
            RemoteDeviceJob::Poll { workspace_id } => self.poll(workspace_id),
            RemoteDeviceJob::Revoke {
                request_id,
                workspace_id,
                device_id,
            } => self.revoke(request_id, &workspace_id, &device_id),
            RemoteDeviceJob::ObserveExecution {
                request_id,
                lease,
                session,
                execution_generation,
            } => self.observe_execution(request_id, lease, session, execution_generation),
            RemoteDeviceJob::DeliverInput {
                request_id,
                lease,
                execution,
                operation_id,
                prompt,
                authority,
            } => self.deliver_input(
                request_id,
                lease,
                *execution,
                operation_id,
                prompt,
                authority,
            ),
            RemoteDeviceJob::ReserveLaunch {
                request_id,
                request,
                initial_prompt,
                authority,
            } => self.launch(request_id, request, initial_prompt, authority),
        };
        result.unwrap_or_else(|code| RemoteDeviceOutcome::Failed { request_id, code })
    }
    fn next_query(&mut self) -> Result<u64> {
        self.query = self.query.checked_add(1).ok_or(RemoteDeviceError::Full)?;
        Ok(self.query)
    }
    fn inspect(&mut self, setup: RemoteDeviceSetup) -> Result<RemoteDeviceOutcome> {
        if self.candidates.len() + self.connections.len() >= 32 {
            return Err(RemoteDeviceError::Full);
        }
        if setup.request_id == 0
            || !valid_text(&setup.name, 256)
            || !valid_text(&setup.workspace_id.0, 128)
            || setup.expected_folder.len() > 4096
            || !std::path::Path::new(&setup.expected_folder).is_absolute()
            || setup.expected_folder.chars().any(char::is_control)
            || setup.auth_token.expose().is_empty()
            || setup.auth_token.expose().len() > 4096
            || setup.endpoint.port() == 0
            || !valid_pin(&setup.pinned_fingerprint)
        {
            return Err(RemoteDeviceError::Invalid);
        }
        let client = RemoteRuntimeClient::attach_tls(
            setup.endpoint,
            setup.auth_token.expose(),
            &setup.pinned_fingerprint,
        )
        .map_err(|_| RemoteDeviceError::Untrusted)?;
        let events = client.subscribe();
        self.epoch = self.epoch.checked_add(1).ok_or(RemoteDeviceError::Full)?;
        let mut connection = Connection {
            device_id: String::new(),
            workspace: setup.workspace_id.clone(),
            client,
            events,
            epoch: self.epoch,
            pin: setup.pinned_fingerprint.clone(),
            last_observed: Instant::now(),
            pending_inspect: None,
            deferred: vec![],
        };
        let caps = inspect_exact(&mut connection, self.next_query()?)?;
        if caps.workspace_id != setup.workspace_id || caps.folder != setup.expected_folder {
            return Err(RemoteDeviceError::WrongTarget);
        }
        connection.device_id = caps.device_id.clone();
        let view = RemoteDeviceCandidate {
            candidate_id: uuid::Uuid::new_v4().to_string(),
            device_id: caps.device_id,
            name: setup.name,
            endpoint: setup.endpoint,
            pinned_fingerprint: setup.pinned_fingerprint,
            workspace_id: caps.workspace_id,
            folder: caps.folder,
            folder_anchor: caps.folder_anchor,
            grant_epoch: caps.grant_epoch,
            incarnation: caps.incarnation,
            profiles: caps
                .profiles
                .into_iter()
                .map(|p| WorkerProfile {
                    id: p.id,
                    agent: p.agent,
                })
                .collect(),
            max_workers: caps.max_workers,
            active_workers: caps.active_workers,
            expires_in_ms: 300_000,
        };
        self.candidates.push(Candidate {
            view: view.clone(),
            connection,
            token: setup.auth_token,
            created: Instant::now(),
        });
        Ok(RemoteDeviceOutcome::Candidate {
            request_id: setup.request_id,
            candidate: view,
        })
    }
    fn grant(
        &mut self,
        request_id: u64,
        candidate_id: &str,
        profiles: Vec<String>,
        max_workers: u8,
    ) -> Result<RemoteDeviceOutcome> {
        let index = self
            .candidates
            .iter()
            .position(|c| c.view.candidate_id == candidate_id)
            .ok_or(RemoteDeviceError::Stale)?;
        if profiles.is_empty() || profiles.len() > 32 || !(1..=8).contains(&max_workers) {
            return Err(RemoteDeviceError::Invalid);
        }
        let query = self.next_query()?;
        let candidate = &mut self.candidates[index];
        let caps = inspect_exact(&mut candidate.connection, query)?;
        if caps.device_id != candidate.view.device_id
            || caps.workspace_id != candidate.view.workspace_id
            || caps.folder != candidate.view.folder
            || caps.folder_anchor != candidate.view.folder_anchor
            || caps.grant_epoch != candidate.view.grant_epoch
            || caps.incarnation != candidate.view.incarnation
            || max_workers > caps.max_workers
        {
            return Err(RemoteDeviceError::WrongTarget);
        }
        let mut agents = vec![];
        for (i, id) in profiles.iter().enumerate() {
            if profiles[..i].contains(id) {
                return Err(RemoteDeviceError::Invalid);
            }
            let profile = caps
                .profiles
                .iter()
                .find(|p| p.id == *id)
                .ok_or(RemoteDeviceError::Invalid)?;
            if !agents.contains(&profile.agent) {
                agents.push(profile.agent.clone());
            }
        }
        let registration = RegisteredDevice {
            id: candidate.view.device_id.clone(),
            name: candidate.view.name.clone(),
            endpoint: candidate.view.endpoint,
            pinned_fingerprint: candidate.view.pinned_fingerprint.clone(),
            workspace_id: candidate.view.workspace_id.clone(),
            folder: candidate.view.folder.clone(),
            folder_anchor: candidate.view.folder_anchor.clone(),
            allowed_agents: agents,
            max_workers,
            grant_epoch: candidate.view.grant_epoch,
            execution_granted: true,
        };
        registration
            .validate()
            .map_err(|_| RemoteDeviceError::Invalid)?;
        let workspace = registration.workspace_id.clone();
        self.load(&workspace)?;
        let previous = self
            .records
            .iter()
            .find(|r| {
                r.registration.id == registration.id && r.registration.workspace_id == workspace
            })
            .cloned();
        self.registry
            .check_registration_update(&registration)
            .map_err(|_| RemoteDeviceError::Conflict)?;
        let candidate = self.candidates.remove(index);
        let secret_id = format!("cc-device-token-{}", candidate.view.candidate_id);
        self.secrets
            .set_secret(&secret_id, &candidate.token)
            .map_err(|_| RemoteDeviceError::Credential)?;
        let stored = match self.store()?.save_execution_device(
            &registration,
            &secret_id,
            previous.as_ref().map(|r| r.revision),
        ) {
            Ok(stored) => stored,
            Err(_) => {
                let _ = self.secrets.delete_secret(&secret_id);
                return Err(RemoteDeviceError::Persistence);
            }
        };
        // The live fence changes only after the sidecar transaction succeeds.
        self.registry
            .register(registration.clone())
            .map_err(|_| RemoteDeviceError::Conflict)?;
        if let Some(old) = self
            .connections
            .iter()
            .position(|c| c.device_id == registration.id && c.workspace == workspace)
        {
            let old = self.connections.remove(old);
            self.mark_unknown(&old.device_id, old.epoch);
        }
        let now = self.now();
        self.registry
            .observe(observation(&candidate.connection, caps), now)
            .map_err(|_| RemoteDeviceError::WrongTarget)?;
        self.connections.push(candidate.connection);
        if let Some(old) = self.records.iter_mut().find(|r| {
            r.registration.id == registration.id && r.registration.workspace_id == workspace
        }) {
            *old = stored.clone();
        } else {
            self.records.push(stored.clone());
        }
        if let Some(previous) = previous
            && previous.token_secret_id != secret_id
        {
            let _ = self.secrets.delete_secret(&previous.token_secret_id);
        }
        Ok(RemoteDeviceOutcome::Granted {
            request_id,
            device: stored,
            devices: self.registry.snapshot(&workspace, self.now()),
            policies: self.registry.verified_devices(&workspace, self.now()),
        })
    }
    fn revoke(
        &mut self,
        request_id: u64,
        workspace: &WorkspaceId,
        device_id: &str,
    ) -> Result<RemoteDeviceOutcome> {
        self.load(workspace)?;
        let index = self
            .records
            .iter()
            .position(|r| {
                r.registration.id == device_id && r.registration.workspace_id == *workspace
            })
            .ok_or(RemoteDeviceError::NotGranted)?;
        let mut registration = self.records[index].registration.clone();
        registration.execution_granted = false;
        let previous = self.records[index].clone();
        let stored = self
            .store()?
            .save_execution_device(
                &registration,
                &previous.token_secret_id,
                Some(previous.revision),
            )
            .map_err(|_| RemoteDeviceError::Persistence)?;
        self.registry.revoke(device_id);
        self.records[index] = stored.clone();
        if let Some(index) = self
            .connections
            .iter()
            .position(|c| c.device_id == device_id && c.workspace == *workspace)
        {
            let connection = self.connections.remove(index);
            self.mark_unknown(device_id, connection.epoch);
        }
        Ok(RemoteDeviceOutcome::Revoked {
            request_id,
            device: stored,
        })
    }
    fn launch(
        &mut self,
        request_id: u64,
        request: LeaseRequest,
        initial_prompt: Option<String>,
        authority: RemoteSendAuthority,
    ) -> Result<RemoteDeviceOutcome> {
        if request_id == 0
            || initial_prompt
                .as_ref()
                .is_some_and(|p| p.is_empty() || p.len() > 16 * 1024 || p.contains('\0'))
        {
            return Err(RemoteDeviceError::Invalid);
        }
        if let Some(flight) = self.flights.iter().find(|f| {
            f.lease.request.operation_id == request.operation_id
                || f.lease.request.attempt_id == request.attempt_id
        }) {
            if flight.lease.request != request || flight.request.initial_prompt != initial_prompt {
                return Err(RemoteDeviceError::Conflict);
            }
            return Ok(match &flight.result {
                Some(result) => RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                    request_id,
                    lease: flight.lease.clone(),
                    result: result.clone(),
                }),
                None => match self
                    .registry
                    .verified_execution_lease(&flight.lease, self.now())
                {
                    Ok(verified) => RemoteDeviceOutcome::LaunchPending {
                        request_id,
                        lease: flight.lease.clone(),
                        verified,
                    },
                    Err(_) => RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                        request_id,
                        lease: flight.lease.clone(),
                        result: RemoteWorkResult::ObservationUnknown,
                    }),
                },
            });
        }
        if self.flights.len() >= 256 {
            return Err(RemoteDeviceError::Full);
        }
        self.load(&request.workspace_id)?;
        let lease = self
            .registry
            .reserve(request, self.now())
            .map_err(|_| RemoteDeviceError::NotGranted)?;
        self.registry
            .validate_lease(&lease, self.now())
            .map_err(|_| RemoteDeviceError::Stale)?;
        let connection = self
            .connections
            .iter()
            .position(|c| {
                c.device_id == lease.request.device_id
                    && c.workspace == lease.request.workspace_id
                    && c.epoch == lease.connection_epoch
            })
            .ok_or(RemoteDeviceError::Disconnected)?;
        let wire = RemoteWorkRequest {
            identity: RemoteWorkIdentity {
                device_id: lease.request.device_id.clone(),
                workspace_id: lease.request.workspace_id.clone(),
                attempt_id: lease.request.attempt_id.clone(),
                operation_id: lease.request.operation_id.clone(),
                incarnation: lease.incarnation.clone(),
                grant_epoch: lease.grant_epoch,
                lease_epoch: lease.lease_epoch,
            },
            folder: lease.request.folder.clone(),
            folder_anchor: lease.folder_anchor.clone(),
            profile_id: lease.profile_id.clone(),
            initial_prompt,
        };
        // Mint before the durable claim/send. No fallible proof conversion may turn an
        // already enqueued launch into the known-unsent Failed outcome.
        let verified = self
            .registry
            .verified_lease(&lease, self.now())
            .map_err(|_| RemoteDeviceError::Stale)?;
        let identity = operation_identity(&wire)?;
        let claim = self
            .store()?
            .claim_remote_operation(&identity)
            .map_err(|_| RemoteDeviceError::Persistence)?;
        let result = match claim {
            RemoteOperationClaim::Conflict => return Err(RemoteDeviceError::Conflict),
            RemoteOperationClaim::Existing(result) => Some(from_stored(result)?),
            RemoteOperationClaim::New => {
                match authority.admit(&wire.identity.operation_id, || {
                    self.connections[connection].client.send_command(
                        RuntimeCommand::SpawnRemoteWork {
                            request: wire.clone(),
                        },
                    )
                }) {
                    Some(Ok(())) => None,
                    Some(Err(_)) => Some(RemoteWorkResult::ObservationUnknown),
                    None => {
                        let rejected = RemoteWorkResult::Rejected {
                            code: runtime::remote_worker::RemoteWorkError::Disabled,
                        };
                        self.store()?
                            .resolve_remote_operation(&identity, &to_stored(&rejected)?)
                            .map_err(|_| RemoteDeviceError::Persistence)?;
                        self.registry.complete(&lease);
                        Some(rejected)
                    }
                }
            }
        };
        self.flights.push(Flight {
            request_id,
            lease: lease.clone(),
            request: wire,
            result: result.clone(),
            pending_result: None,
            execution: None,
        });
        Ok(match result {
            Some(result) => RemoteDeviceOutcome::LaunchReceipt(RemoteLaunchReceipt {
                request_id,
                lease,
                result,
            }),
            None => RemoteDeviceOutcome::LaunchPending {
                request_id,
                verified,
                lease,
            },
        })
    }
    fn exact_connection(&self, lease: &ExecutionLease) -> Result<usize> {
        self.registry
            .validate_execution_lease(lease, self.now())
            .map_err(|_| RemoteDeviceError::Stale)?;
        self.connections
            .iter()
            .position(|c| {
                c.device_id == lease.request.device_id
                    && c.workspace == lease.request.workspace_id
                    && c.epoch == lease.connection_epoch
            })
            .ok_or(RemoteDeviceError::Disconnected)
    }
    fn launched_target(&self, lease: &ExecutionLease) -> Result<RemoteTaskTarget> {
        let flight = self
            .flights
            .iter()
            .find(|f| f.lease == *lease)
            .ok_or(RemoteDeviceError::WrongTarget)?;
        let Some(RemoteWorkResult::Launched {
            session,
            execution_generation,
        }) = flight.result
        else {
            return Err(RemoteDeviceError::WrongTarget);
        };
        Ok(RemoteTaskTarget {
            identity: flight.request.identity.clone(),
            session,
            execution_generation,
        })
    }
    fn observe_execution(
        &mut self,
        request_id: u64,
        lease: ExecutionLease,
        session: deppy_core::SessionId,
        execution_generation: u64,
    ) -> Result<RemoteDeviceOutcome> {
        let target = self.launched_target(&lease)?;
        if request_id == 0
            || target.session != session
            || target.execution_generation != execution_generation
        {
            return Err(RemoteDeviceError::WrongTarget);
        }
        let index = self.exact_connection(&lease)?;
        let query = self.next_query()?;
        let mut connection = self.connections.remove(index);
        let result = (|| {
            connection
                .client
                .send_command(RuntimeCommand::RemoteTaskInput {
                    request: RemoteTaskInputRequest::Observe {
                        target: target.clone(),
                        request_id: query,
                    },
                })
                .map_err(|_| RemoteDeviceError::Transport)?;
            let until = Instant::now() + Duration::from_secs(5);
            loop {
                let mut events = connection.events.drain().into_iter();
                while let Some(event) = events.next() {
                    match event {
                        RuntimeEvent::RemoteTaskInput {
                            target: observed,
                            reply:
                                RemoteTaskInputReply::Observed {
                                    request_id,
                                    execution,
                                },
                        } if observed == target && request_id == query => {
                            if execution.as_ref().is_some_and(|e| {
                                e.target != target
                                    || e.profile != lease.request.agent
                                    || uuid::Uuid::parse_str(&e.execution_handle).is_err()
                            }) {
                                return Err(RemoteDeviceError::WrongTarget);
                            }
                            let remaining: Vec<_> = events.collect();
                            if connection.deferred.len() + remaining.len() > 256 {
                                return Err(RemoteDeviceError::Full);
                            }
                            connection.deferred.extend(remaining);
                            return Ok(execution.map(|proof| *proof));
                        }
                        other => {
                            if connection.deferred.len() >= 256 {
                                return Err(RemoteDeviceError::Full);
                            }
                            connection.deferred.push(other);
                        }
                    }
                }
                if connection.events.is_disconnected()
                    || connection.events.take_overflowed()
                    || Instant::now() >= until
                {
                    return Err(RemoteDeviceError::Disconnected);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        self.connections.insert(index, connection);
        let execution = result?;
        if let Some(flight) = self.flights.iter_mut().find(|f| f.lease == lease) {
            flight.execution = execution.clone();
        }
        Ok(RemoteDeviceOutcome::ExecutionObserved {
            request_id,
            verified: execution.as_ref().and_then(|_| {
                self.registry
                    .verified_execution_lease(&lease, self.now())
                    .ok()
            }),
            execution: execution.map(Box::new),
            lease,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn deliver_input(
        &mut self,
        request_id: u64,
        lease: ExecutionLease,
        execution: RemoteExecutionProof,
        operation_id: OperationId,
        prompt: String,
        authority: RemoteSendAuthority,
    ) -> Result<RemoteDeviceOutcome> {
        if request_id == 0 {
            return Err(RemoteDeviceError::Invalid);
        }
        let target = self.launched_target(&lease)?;
        if execution.target != target
            || !self
                .flights
                .iter()
                .any(|f| f.lease == lease && f.execution.as_ref() == Some(&execution))
        {
            return Err(RemoteDeviceError::WrongTarget);
        }
        if let Some(input) = self.inputs.iter().find(|i| i.operation_id == operation_id) {
            if input.lease != lease
                || !matches!(&input.request,RemoteTaskInputRequest::Deliver{prompt:old,..} if *old==prompt)
            {
                return Err(RemoteDeviceError::Conflict);
            }
            return Ok(input
                .result
                .as_ref()
                .filter(|r| {
                    !matches!(r, RemoteTaskInputResult::Admitted) || input.accepted.is_some()
                })
                .map_or_else(
                    || RemoteDeviceOutcome::InputPending {
                        request_id,
                        lease: lease.clone(),
                        operation_id: operation_id.clone(),
                    },
                    |_| RemoteDeviceOutcome::InputReceipt(input_receipt(input)),
                ));
        }
        if self.inputs.len() >= 256 {
            return Err(RemoteDeviceError::Full);
        }
        let index = self.exact_connection(&lease)?;
        let request = RemoteTaskInputRequest::Deliver {
            target,
            execution_handle: execution.execution_handle,
            operation_id: operation_id.clone(),
            prompt,
            deadline_unix_ms: authority
                .deadline_unix_ms()
                .ok_or(RemoteDeviceError::Stale)?,
        };
        if !request.is_valid() {
            return Err(RemoteDeviceError::Invalid);
        }
        let result = match authority.admit(&operation_id, || {
            self.connections[index]
                .client
                .send_command(RuntimeCommand::RemoteTaskInput {
                    request: request.clone(),
                })
        }) {
            Some(Ok(())) => None,
            Some(Err(_)) => Some(RemoteTaskInputResult::ObservationUnknown),
            None => Some(RemoteTaskInputResult::Rejected {
                reason: runtime::PtyInputRejectReason::AdmissionDenied,
            }),
        };
        self.inputs.push(InputFlight {
            request_id,
            lease: lease.clone(),
            request,
            operation_id: operation_id.clone(),
            result: result.clone(),
            accepted: None,
            emitted: result.is_some(),
            started: Instant::now(),
        });
        Ok(if result.is_some() {
            RemoteDeviceOutcome::InputReceipt(input_receipt(
                self.inputs.last().ok_or(RemoteDeviceError::Conflict)?,
            ))
        } else {
            RemoteDeviceOutcome::InputPending {
                request_id,
                lease,
                operation_id,
            }
        })
    }
    fn mark_unknown(&mut self, device: &str, epoch: u64) {
        self.registry.disconnect(device, epoch);
        for input in &mut self.inputs {
            if input.lease.request.device_id == device
                && input.lease.connection_epoch == epoch
                && !input.emitted
            {
                input.result = Some(RemoteTaskInputResult::ObservationUnknown);
            }
        }
        for flight in &mut self.flights {
            if flight.lease.request.device_id == device
                && flight.lease.connection_epoch == epoch
                && matches!(
                    flight.result,
                    None | Some(RemoteWorkResult::Launched { .. })
                )
            {
                flight.pending_result = None;
                flight.result = Some(RemoteWorkResult::ObservationUnknown);
            }
        }
    }
    fn poll(&mut self, workspace: WorkspaceId) -> Result<RemoteDeviceOutcome> {
        self.load(&workspace)?;
        let mut receipts = vec![];
        let count = self.connections.len().min(4);
        for _ in 0..count {
            if self.connections.is_empty() {
                break;
            }
            let index = self.poll_cursor % self.connections.len();
            self.poll_cursor = self.poll_cursor.wrapping_add(1);
            let mut connection = self.connections.remove(index);
            if connection.events.is_disconnected() || connection.events.take_overflowed() {
                self.mark_unknown(&connection.device_id, connection.epoch);
                for flight in &self.flights {
                    if flight.lease.request.device_id == connection.device_id
                        && flight.lease.connection_epoch == connection.epoch
                        && flight.result == Some(RemoteWorkResult::ObservationUnknown)
                    {
                        receipts.push(receipt(flight));
                    }
                }
                continue;
            }
            let events = std::mem::take(&mut connection.deferred)
                .into_iter()
                .chain(connection.events.drain());
            for event in events {
                match event {
                    RuntimeEvent::RemoteWorkerObserved {
                        request_id,
                        capabilities,
                    } => {
                        if connection
                            .pending_inspect
                            .as_ref()
                            .is_some_and(|(id, _)| *id == request_id)
                        {
                            connection.pending_inspect = None;
                            if let Some(caps) = capabilities {
                                let now = self.now();
                                if self
                                    .registry
                                    .observe(observation(&connection, caps), now)
                                    .is_err()
                                {
                                    self.mark_unknown(&connection.device_id, connection.epoch);
                                } else {
                                    connection.last_observed = Instant::now();
                                }
                            } else {
                                self.mark_unknown(&connection.device_id, connection.epoch);
                            }
                        }
                    }
                    RuntimeEvent::RemoteTaskInput {
                        target,
                        reply:
                            RemoteTaskInputReply::Delivered {
                                operation_id,
                                result,
                            },
                    } => {
                        if let Some(input) = self.inputs.iter_mut().find(|i| {
                            i.operation_id == operation_id
                                && *i.request.target() == target
                                && i.lease.connection_epoch == connection.epoch
                                && i.lease.request.device_id == connection.device_id
                                && !i.emitted
                        }) {
                            input.result = Some(result);
                        }
                    }
                    RuntimeEvent::SessionInputAccepted {
                        session,
                        operation_id: Some(operation),
                        sequence,
                        at_micros,
                    } => {
                        if sequence > 0
                            && at_micros > 0
                            && let Some(input) = self.inputs.iter_mut().find(|i| {
                                i.operation_id.as_str() == operation
                                    && i.request.target().session == session
                                    && i.lease.connection_epoch == connection.epoch
                                    && i.lease.request.device_id == connection.device_id
                                    && !i.emitted
                            })
                        {
                            input.accepted = Some(RemoteInputAcceptance {
                                sequence,
                                at_micros,
                            });
                        }
                    }
                    RuntimeEvent::RemoteWorkResolved { identity, result } => {
                        if let Some(flight) = self.flights.iter_mut().find(|f| {
                            f.request.identity == identity
                                && f.lease.connection_epoch == connection.epoch
                                && f.lease.request.device_id == connection.device_id
                                && !matches!(f.result, Some(RemoteWorkResult::ObservationUnknown))
                        }) {
                            flight.pending_result = Some(result);
                        }
                    }
                    _ => {}
                }
            }
            if connection
                .pending_inspect
                .as_ref()
                .is_some_and(|(_, sent)| sent.elapsed() > Duration::from_secs(5))
            {
                self.mark_unknown(&connection.device_id, connection.epoch);
                connection.pending_inspect = None;
            }
            if connection.last_observed.elapsed() > Duration::from_secs(5)
                && connection.pending_inspect.is_none()
            {
                let id = self.next_query()?;
                if connection
                    .client
                    .send_command(RuntimeCommand::InspectRemoteWorker { request_id: id })
                    .is_err()
                {
                    self.mark_unknown(&connection.device_id, connection.epoch);
                } else {
                    connection.pending_inspect = Some((id, Instant::now()));
                }
            }
            self.connections.insert(index, connection);
        }
        for index in 0..self.flights.len() {
            let Some(result) = self.flights[index].pending_result.take() else {
                continue;
            };
            let request = self.flights[index].request.clone();
            if matches!(result, RemoteWorkResult::ObservationUnknown) {
                self.flights[index].result = Some(result);
                receipts.push(receipt(&self.flights[index]));
                continue;
            }
            let identity = operation_identity(&request)?;
            let stored = to_stored(&result)?;
            if self
                .store()?
                .resolve_remote_operation(&identity, &stored)
                .is_err()
            {
                self.flights[index].pending_result = Some(result);
                continue;
            }
            if matches!(result, RemoteWorkResult::Rejected { .. }) {
                self.registry.complete(&self.flights[index].lease);
            }
            self.flights[index].result = Some(result);
            receipts.push(receipt(&self.flights[index]));
        }
        let mut inputs = vec![];
        for input in &mut self.inputs {
            if input.emitted {
                continue;
            }
            if input.started.elapsed() > Duration::from_secs(5)
                && (input.result.is_none()
                    || (input.result == Some(RemoteTaskInputResult::Admitted)
                        && input.accepted.is_none()))
            {
                input.result = Some(RemoteTaskInputResult::ObservationUnknown);
            }
            if input
                .result
                .as_ref()
                .is_some_and(|r| *r != RemoteTaskInputResult::Admitted || input.accepted.is_some())
            {
                input.emitted = true;
                inputs.push(input_receipt(input));
            }
        }
        Ok(RemoteDeviceOutcome::Snapshot {
            devices: self.registry.snapshot(&workspace, self.now()),
            policies: self.registry.verified_devices(&workspace, self.now()),
            workspace_id: workspace,
            receipts,
            inputs,
        })
    }
}
fn input_receipt(input: &InputFlight) -> RemoteInputReceipt {
    RemoteInputReceipt {
        request_id: input.request_id,
        lease: input.lease.clone(),
        operation_id: input.operation_id.clone(),
        result: input
            .result
            .clone()
            .unwrap_or(RemoteTaskInputResult::ObservationUnknown),
        accepted: input.accepted.clone(),
    }
}
fn receipt(flight: &Flight) -> RemoteLaunchReceipt {
    RemoteLaunchReceipt {
        request_id: flight.request_id,
        lease: flight.lease.clone(),
        result: flight
            .result
            .clone()
            .unwrap_or(RemoteWorkResult::ObservationUnknown),
    }
}
fn valid_text(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}
fn valid_pin(pin: &str) -> bool {
    pin.len() == 95
        && pin.bytes().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                b == b':'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        })
}
fn inspect_exact(connection: &mut Connection, id: u64) -> Result<RemoteWorkerCapabilities> {
    connection
        .client
        .send_command(RuntimeCommand::InspectRemoteWorker { request_id: id })
        .map_err(|_| RemoteDeviceError::Transport)?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        for event in connection.events.drain() {
            if let RuntimeEvent::RemoteWorkerObserved {
                request_id,
                capabilities,
            } = event
                && request_id == id
            {
                let caps = capabilities
                    .filter(RemoteWorkerCapabilities::is_valid)
                    .ok_or(RemoteDeviceError::NotGranted)?;
                connection.last_observed = Instant::now();
                return Ok(caps);
            }
        }
        if connection.events.is_disconnected() || connection.events.take_overflowed() {
            return Err(RemoteDeviceError::Disconnected);
        }
        if Instant::now() >= until {
            return Err(RemoteDeviceError::Stale);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn observation(connection: &Connection, caps: RemoteWorkerCapabilities) -> WorkerObservation {
    WorkerObservation {
        device_id: caps.device_id,
        pinned_fingerprint: connection.pin.clone(),
        connection_epoch: connection.epoch,
        incarnation: caps.incarnation,
        workspace_id: caps.workspace_id,
        folder: caps.folder,
        folder_anchor: caps.folder_anchor,
        grant_epoch: caps.grant_epoch,
        lease_epoch: caps.lease_epoch,
        profiles: caps
            .profiles
            .into_iter()
            .map(|p| WorkerProfile {
                id: p.id,
                agent: p.agent,
            })
            .collect(),
        max_workers: caps.max_workers,
        active_workers: caps.active_workers,
        active_operation_ids: caps.active_operation_ids,
    }
}
fn operation_identity(request: &RemoteWorkRequest) -> Result<RemoteOperationIdentity> {
    Ok(RemoteOperationIdentity {
        workspace_id: request.identity.workspace_id.clone(),
        device_id: request.identity.device_id.clone(),
        attempt_id: request.identity.attempt_id.clone(),
        operation_id: request.identity.operation_id.clone(),
        worker_incarnation: request.identity.incarnation.clone(),
        request_digest: request.digest().map_err(|_| RemoteDeviceError::Invalid)?,
        grant_epoch: request.identity.grant_epoch,
        lease_epoch: request.identity.lease_epoch,
    })
}
fn to_stored(result: &RemoteWorkResult) -> Result<RemoteOperationResult> {
    Ok(match result {
        RemoteWorkResult::Launched {
            session,
            execution_generation,
        } if session.0 > 0 && *execution_generation > 0 => RemoteOperationResult::Launched {
            session_id: session.0,
            execution_generation: *execution_generation,
        },
        RemoteWorkResult::Rejected { code } => RemoteOperationResult::Rejected {
            code: serde_json::to_string(code).map_err(|_| RemoteDeviceError::Invalid)?,
        },
        RemoteWorkResult::ObservationUnknown => RemoteOperationResult::ObservationUnknown,
        _ => return Err(RemoteDeviceError::Invalid),
    })
}
fn from_stored(result: RemoteOperationResult) -> Result<RemoteWorkResult> {
    Ok(match result {
        RemoteOperationResult::Launched {
            session_id,
            execution_generation,
        } => RemoteWorkResult::Launched {
            session: deppy_core::SessionId(session_id),
            execution_generation,
        },
        RemoteOperationResult::Rejected { code } => RemoteWorkResult::Rejected {
            code: serde_json::from_str(&code).map_err(|_| RemoteDeviceError::Invalid)?,
        },
        RemoteOperationResult::ObservationUnknown => RemoteWorkResult::ObservationUnknown,
    })
}
