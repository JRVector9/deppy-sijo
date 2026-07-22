use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use auth::{
    AuthorizationServerMetadata, McpOAuthChallenge, RegistrationError, RegistrationOptions,
    StoredOAuthMetadata, StoredOAuthMetadataDraft, StoredOAuthMetadataLimits,
    TokenEndpointAuthMethod,
};
use connector_contract::{EndpointDisplay, ErrorCode, OperationId, ServerDraft, TransportDraft};

use crate::{
    CancellationToken, ConnectorOAuth, OAuthAuthorizeOutput, OAuthClientRequest, OAuthCompletion,
    OAuthContinuation, OAuthCredentialUpdate, OAuthDiscovery, OAuthEventSink, OAuthFailure,
    OAuthRecoveryTarget, ServiceError, StoredOAuthClient,
};

const DEFAULT_CLIENT_NAME: &str = "Deppy Sijo Connector";

pub struct ProductionConnectorOAuth {
    request_timeout: Duration,
    callback_timeout: Duration,
    protocol_version: String,
    client_name: String,
}

impl ProductionConnectorOAuth {
    pub fn new(
        request_timeout: Duration,
        callback_timeout: Duration,
        protocol_version: impl Into<String>,
    ) -> Result<Self, ServiceError> {
        let protocol_version = compact_string(protocol_version.into());
        if request_timeout.is_zero()
            || callback_timeout.is_zero()
            || protocol_version.is_empty()
            || protocol_version.len() > 64
            || !protocol_version.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ServiceError::new(
                ErrorCode::InvalidInput,
                "OAuth adapter configuration is invalid",
            ));
        }
        Ok(Self {
            request_timeout,
            callback_timeout,
            protocol_version,
            client_name: DEFAULT_CLIENT_NAME.to_owned(),
        })
    }
}

struct ProductionContinuation {
    server: ServerDraft,
    provider: AuthorizationServerMetadata,
    scopes: Vec<String>,
    oauth_resource: String,
    choose_workspace: bool,
    stored_client: Option<ProductionClient>,
}

struct ProductionClient {
    server_id: connector_contract::ServerId,
    logical_id: secret::LogicalCredentialId,
    client_id: String,
    client_secret: Option<connector_contract::SensitiveInput>,
    workspace_hint: Option<String>,
    provider_workspace_id: Option<String>,
    manual_client: bool,
}

impl ProductionContinuation {
    fn compact(mut self) -> Result<Self, ServiceError> {
        self.server = compact_server(self.server);
        self.provider = compact_provider(self.provider);
        self.scopes = compact_string_vec(self.scopes);
        self.oauth_resource = compact_string(self.oauth_resource);
        self.stored_client = self.stored_client.map(ProductionClient::compact);
        self.retained_bytes()?;
        Ok(self)
    }

    fn retained_bytes(&self) -> Result<usize, ServiceError> {
        let (server_bytes, mut items) = server_allocation(&self.server)?;
        // `OAuthContinuationState::Typed` stores this value behind a type-erased Box. Count the
        // concrete payload allocation as well as every nested heap allocation below.
        let mut bytes = checked_add(std::mem::size_of::<Self>(), server_bytes)?;
        bytes = checked_add(bytes, self.oauth_resource.capacity())?;
        for value in [
            &self.provider.issuer,
            &self.provider.authorization_endpoint,
            &self.provider.token_endpoint,
        ] {
            bytes = checked_add(bytes, value.capacity())?;
        }
        if let Some(endpoint) = &self.provider.registration_endpoint {
            bytes = checked_add(bytes, endpoint.capacity())?;
        }
        let (scope_bytes, scope_items) =
            string_vec_allocation(&self.scopes, self.scopes.capacity())?;
        bytes = checked_add(bytes, scope_bytes)?;
        items = checked_add(items, scope_items)?;
        for values in [
            self.provider.grant_types_supported.as_ref(),
            self.provider.scopes_supported.as_ref(),
            self.provider.token_endpoint_auth_methods_supported.as_ref(),
            self.provider.code_challenge_methods_supported.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            let (value_bytes, value_items) = string_vec_allocation(values, values.capacity())?;
            bytes = checked_add(bytes, value_bytes)?;
            items = checked_add(items, value_items)?;
        }
        if let Some(client) = &self.stored_client {
            bytes = checked_add(bytes, client.server_id.as_str().len())?;
            bytes = checked_add(bytes, client.logical_id.as_str().len())?;
            bytes = checked_add(bytes, client.client_id.capacity())?;
            bytes = checked_add(
                bytes,
                client
                    .client_secret
                    .as_ref()
                    .map_or(0, |secret| secret.len()),
            )?;
            bytes = checked_add(
                bytes,
                client.workspace_hint.as_ref().map_or(0, String::capacity),
            )?;
            bytes = checked_add(
                bytes,
                client
                    .provider_workspace_id
                    .as_ref()
                    .map_or(0, String::capacity),
            )?;
        }
        if bytes > StoredOAuthMetadataLimits::PRODUCTION.json_bytes
            || items
                > StoredOAuthMetadataLimits::PRODUCTION
                    .scopes
                    .saturating_mul(4)
        {
            return Err(ServiceError::new(
                ErrorCode::LimitExceeded,
                "OAuth continuation exceeds its byte limit",
            ));
        }
        Ok(bytes)
    }
}

impl ProductionClient {
    fn from_stored(
        client: StoredOAuthClient,
        expected_server_id: &connector_contract::ServerId,
        exact_server_url: &str,
    ) -> Result<Self, ServiceError> {
        let (manual_client, provider_workspace_id) = match client.metadata.as_ref() {
            Some(metadata) => {
                metadata
                    .validate_for_binding(
                        expected_server_id.as_str(),
                        exact_server_url,
                        StoredOAuthMetadataLimits::PRODUCTION,
                    )
                    .map_err(|_| invalid_stored_binding())?;
                if client.client_id != metadata.client_id() {
                    return Err(invalid_stored_binding());
                }
                (
                    persisted_manual_client(client.manual_client, Some(metadata)),
                    metadata
                        .provider_workspace_id()
                        .map(str::to_owned)
                        .map(compact_string),
                )
            }
            None => (client.manual_client, None),
        };
        if client.server_id != *expected_server_id {
            return Err(invalid_stored_binding());
        }
        let logical_id = secret::LogicalCredentialId::new(client.logical_id.as_str().to_owned())
            .map_err(|_| invalid_stored_binding())?;
        Ok(Self {
            server_id: connector_contract::ServerId::new(client.server_id.as_str().to_owned()),
            logical_id,
            client_id: compact_string(client.client_id),
            client_secret: client
                .client_secret
                .map(|secret| connector_contract::SensitiveInput::new(secret.into_bytes())),
            workspace_hint: client.workspace_hint.map(compact_string),
            provider_workspace_id,
            manual_client,
        }
        .compact())
    }

    fn compact(mut self) -> Self {
        self.server_id =
            connector_contract::ServerId::new(compact_string(self.server_id.as_str().to_owned()));
        self.logical_id =
            secret::LogicalCredentialId::new(compact_string(self.logical_id.as_str().to_owned()))
                .expect("validated OAuth logical credential id");
        self.client_id = compact_string(self.client_id);
        self.client_secret = self
            .client_secret
            .map(|secret| connector_contract::SensitiveInput::new(secret.into_bytes()));
        self.workspace_hint = self.workspace_hint.map(compact_string);
        self.provider_workspace_id = self.provider_workspace_id.map(compact_string);
        self
    }
}

fn compact_string(mut value: String) -> String {
    value.shrink_to_fit();
    value
}

fn compact_string_vec(mut values: Vec<String>) -> Vec<String> {
    for value in &mut values {
        value.shrink_to_fit();
    }
    values.shrink_to_fit();
    values
}

fn compact_provider(provider: AuthorizationServerMetadata) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: compact_string(provider.issuer),
        authorization_endpoint: compact_string(provider.authorization_endpoint),
        token_endpoint: compact_string(provider.token_endpoint),
        registration_endpoint: provider.registration_endpoint.map(compact_string),
        grant_types_supported: provider.grant_types_supported.map(compact_string_vec),
        scopes_supported: provider.scopes_supported.map(compact_string_vec),
        token_endpoint_auth_methods_supported: provider
            .token_endpoint_auth_methods_supported
            .map(compact_string_vec),
        code_challenge_methods_supported: provider
            .code_challenge_methods_supported
            .map(compact_string_vec),
    }
}

fn compact_server(server: ServerDraft) -> ServerDraft {
    let ServerDraft {
        id,
        name,
        transport,
        enabled,
    } = server;
    let id = id.map(|id| {
        let value: String = id.into();
        connector_contract::ServerId::new(compact_string(value))
    });
    let transport = match transport {
        TransportDraft::Http { url } => TransportDraft::Http {
            url: compact_string(url),
        },
        TransportDraft::Stdio {
            command,
            args,
            plain_env,
            secret_env,
            inherit_env,
        } => TransportDraft::Stdio {
            command: compact_string(command),
            args: compact_string_vec(args),
            plain_env: compact_string_pairs(plain_env),
            secret_env: compact_credential_pairs(secret_env),
            inherit_env,
        },
    };
    ServerDraft {
        id,
        name: compact_string(name),
        transport,
        enabled,
    }
}

fn compact_string_pairs(mut values: Vec<(String, String)>) -> Vec<(String, String)> {
    for (key, value) in &mut values {
        key.shrink_to_fit();
        value.shrink_to_fit();
    }
    values.shrink_to_fit();
    values
}

fn compact_credential_pairs(
    values: Vec<(String, connector_contract::CredentialId)>,
) -> Vec<(String, connector_contract::CredentialId)> {
    let mut values = values
        .into_iter()
        .map(|(key, credential_id)| {
            let credential_id: String = credential_id.into();
            (
                compact_string(key),
                connector_contract::CredentialId::new(compact_string(credential_id)),
            )
        })
        .collect::<Vec<_>>();
    values.shrink_to_fit();
    values
}

fn string_vec_allocation(
    values: &[String],
    vector_capacity: usize,
) -> Result<(usize, usize), ServiceError> {
    let mut bytes = checked_mul(vector_capacity, std::mem::size_of::<String>())?;
    for value in values {
        bytes = checked_add(bytes, value.capacity())?;
    }
    Ok((bytes, values.len()))
}

fn server_allocation(server: &ServerDraft) -> Result<(usize, usize), ServiceError> {
    let mut bytes = server.name.capacity();
    let mut items = 0usize;
    if let Some(id) = &server.id {
        bytes = checked_add(bytes, id.as_str().len())?;
    }
    match &server.transport {
        TransportDraft::Http { url } => {
            bytes = checked_add(bytes, url.capacity())?;
        }
        TransportDraft::Stdio {
            command,
            args,
            plain_env,
            secret_env,
            ..
        } => {
            bytes = checked_add(bytes, command.capacity())?;
            let (argument_bytes, argument_items) = string_vec_allocation(args, args.capacity())?;
            bytes = checked_add(bytes, argument_bytes)?;
            items = checked_add(items, argument_items)?;
            bytes = checked_add(
                bytes,
                checked_mul(
                    plain_env.capacity(),
                    std::mem::size_of::<(String, String)>(),
                )?,
            )?;
            for (key, value) in plain_env {
                bytes = checked_add(bytes, key.capacity())?;
                bytes = checked_add(bytes, value.capacity())?;
            }
            items = checked_add(items, plain_env.len())?;
            bytes = checked_add(
                bytes,
                checked_mul(
                    secret_env.capacity(),
                    std::mem::size_of::<(String, connector_contract::CredentialId)>(),
                )?,
            )?;
            for (key, credential_id) in secret_env {
                bytes = checked_add(bytes, key.capacity())?;
                bytes = checked_add(bytes, credential_id.as_str().len())?;
            }
            items = checked_add(items, secret_env.len())?;
        }
    }
    Ok((bytes, items))
}

fn discovery_retained_bytes(
    state: &ProductionContinuation,
    authority_capacity: usize,
    resource_capacity: usize,
    scopes: &[String],
    scope_capacity: usize,
) -> Result<usize, ServiceError> {
    let mut bytes = state.retained_bytes()?;
    bytes = checked_add(bytes, authority_capacity)?;
    bytes = checked_add(bytes, resource_capacity)?;
    bytes = checked_add(bytes, string_vec_allocation(scopes, scope_capacity)?.0)?;
    // `finish_oauth_discovery` moves this vector into an `Arc<[String]>`. Its elements are counted
    // above; keep a conservative two-word allocation header for the snapshot collection.
    bytes = checked_add(bytes, checked_mul(2, std::mem::size_of::<usize>())?)?;
    enforce_continuation_limit(bytes)?;
    Ok(bytes)
}

impl ConnectorOAuth for ProductionConnectorOAuth {
    fn discover(
        &self,
        _operation_id: &OperationId,
        server: ServerDraft,
        choose_workspace: bool,
        mut stored_client: Option<StoredOAuthClient>,
        cancellation: CancellationToken,
    ) -> Result<OAuthDiscovery, ServiceError> {
        let server_id = server.id.as_ref().ok_or_else(|| {
            ServiceError::new(ErrorCode::InvalidInput, "OAuth connector id is missing")
        })?;
        let server_url = match &server.transport {
            TransportDraft::Http { url } => url,
            TransportDraft::Stdio { .. } => {
                return Err(ServiceError::new(
                    ErrorCode::InvalidInput,
                    "OAuth requires an HTTP connector",
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }

        let (provider, scopes, oauth_resource) = if let Some(client) = stored_client.as_ref() {
            let metadata = client.metadata.as_ref().ok_or_else(|| {
                ServiceError::new(
                    ErrorCode::AuthenticationFailed,
                    "stored OAuth client metadata is missing",
                )
            })?;
            metadata
                .validate_for_binding(
                    server_id.as_str(),
                    server_url,
                    StoredOAuthMetadataLimits::PRODUCTION,
                )
                .map_err(|_| invalid_stored_binding())?;
            if client.server_id != *server_id || client.client_id != metadata.client_id() {
                return Err(invalid_stored_binding());
            }
            (
                provider_from_stored(metadata),
                metadata.scopes().to_vec(),
                metadata.oauth_resource().to_owned(),
            )
        } else {
            let discovered = auth::discover_mcp_oauth_cancellable(
                self.request_timeout,
                server_url,
                &McpOAuthChallenge::new(None, None),
                &self.protocol_version,
                StoredOAuthMetadataLimits::PRODUCTION,
                || cancellation.is_cancelled(),
            )
            .map_err(map_primitive_error)?;
            let (provider, scopes, oauth_resource) = discovered.into_parts();
            (provider, scopes, oauth_resource)
        };
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        // Discard an unusable stored secret before it can reach a token endpoint.
        if stored_client.as_ref().is_some_and(|client| {
            client.metadata.as_ref().is_some_and(|metadata| {
                metadata.token_endpoint_auth_method().uses_client_secret()
                    != client.client_secret.is_some()
            })
        }) {
            stored_client = None;
        }
        let stored_client = stored_client
            .map(|client| ProductionClient::from_stored(client, server_id, server_url))
            .transpose()?;
        let authority = compact_string(
            auth::oauth_authority_display(&provider.issuer).map_err(map_primitive_error)?,
        );
        let state = ProductionContinuation {
            server,
            provider,
            scopes: scopes.clone(),
            oauth_resource: oauth_resource.clone(),
            choose_workspace,
            stored_client,
        }
        .compact()?;
        let scopes = compact_string_vec(scopes);
        let oauth_resource = compact_string(oauth_resource);
        let retained_bytes = discovery_retained_bytes(
            &state,
            authority.capacity(),
            oauth_resource.capacity(),
            &scopes,
            scopes.capacity(),
        )?;
        Ok(OAuthDiscovery {
            continuation: OAuthContinuation::typed(state, retained_bytes),
            authority: EndpointDisplay::new(authority),
            resource: EndpointDisplay::new(oauth_resource),
            scopes,
        })
    }

    fn authorize(
        &self,
        _operation_id: &OperationId,
        continuation: OAuthContinuation,
        client: Option<StoredOAuthClient>,
        workspace: Option<String>,
        events: Arc<dyn OAuthEventSink>,
        cancellation: CancellationToken,
    ) -> Result<OAuthAuthorizeOutput, ServiceError> {
        let mut state = continuation.into_typed::<ProductionContinuation>()?;
        let server_id = state.server.id.as_ref().ok_or_else(|| {
            ServiceError::new(ErrorCode::InvalidInput, "OAuth connector id is missing")
        })?;
        let server_url = match &state.server.transport {
            TransportDraft::Http { url } => url,
            TransportDraft::Stdio { .. } => {
                return Err(ServiceError::new(
                    ErrorCode::InvalidInput,
                    "OAuth requires an HTTP connector",
                ));
            }
        };
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }

        let workspace_target = if let Some(workspace) = workspace {
            if server_url.trim_end_matches('/') != auth::SLACK_MCP_URL.trim_end_matches('/') {
                return Ok(failed(ErrorCode::InvalidInput));
            }
            Some(
                auth::resolve_slack_workspace_cancellable(self.request_timeout, &workspace, || {
                    cancellation.is_cancelled()
                })
                .map_err(|error| match error {
                    auth::SlackWorkspaceError::Cancelled => cancelled(),
                    _ => ServiceError::new(
                        ErrorCode::AuthenticationFailed,
                        "Slack workspace resolution failed",
                    ),
                })?,
            )
        } else {
            None
        };

        let client = client
            .map(|client| ProductionClient::from_stored(client, server_id, server_url))
            .transpose()?;
        let mut selected = client.or_else(|| state.stored_client.take());
        if let Some(client) = selected.as_ref()
            && client.server_id != *server_id
        {
            return Ok(failed(ErrorCode::StaleResult));
        }
        if selected.is_none() {
            let registration = run_blocking_with_cancel_checks(&cancellation, || {
                auth::register_client(
                    self.request_timeout,
                    &state.provider,
                    &RegistrationOptions {
                        client_name: self.client_name.clone(),
                        scopes: state.scopes.clone(),
                    },
                )
            })?;
            match registration {
                Ok(registration) => {
                    let logical_id = oauth_logical_id(server_id)?;
                    selected = Some(
                        ProductionClient {
                            server_id: server_id.clone(),
                            logical_id,
                            client_id: compact_string(registration.client_id),
                            client_secret: registration.client_secret.map(secret_to_sensitive),
                            workspace_hint: workspace_target
                                .as_ref()
                                .map(|target| compact_string(target.domain().to_owned())),
                            provider_workspace_id: None,
                            manual_client: false,
                        }
                        .compact(),
                    );
                }
                Err(RegistrationError::Unsupported(_) | RegistrationError::Rejected(_)) => {
                    let retained_bytes = state.retained_bytes()?;
                    return Ok(OAuthAuthorizeOutput::ClientInputRequired(
                        OAuthClientRequest {
                            continuation: OAuthContinuation::typed(state, retained_bytes),
                            reason: ErrorCode::AuthenticationRequired,
                            workspace_hint: workspace_target
                                .as_ref()
                                .map(|target| target.domain().to_owned()),
                        },
                    ));
                }
                Err(RegistrationError::Other(_)) => {
                    return Ok(failed(ErrorCode::AuthenticationFailed));
                }
            }
        }
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let selected = selected.expect("OAuth client selected or returned");
        let ProductionClient {
            logical_id,
            client_id,
            client_secret,
            workspace_hint,
            provider_workspace_id,
            manual_client,
            ..
        } = selected;
        let (flow_secret, stored_dcr_secret) = split_client_secret(client_secret)?;
        let slack_team_id = workspace_target
            .as_ref()
            .map(|target| target.team_id().to_owned())
            .or_else(|| provider_workspace_id.clone());
        let prepared = auth::prepare_oauth_provider_config(
            &state.provider,
            client_id.clone(),
            flow_secret,
            state.scopes.clone(),
            slack_team_id,
            StoredOAuthMetadataLimits::PRODUCTION,
        )
        .map_err(map_primitive_error)?;
        let auth_method = prepared.auth_method();
        let callback = auth::bind_fixed_localhost_cancellable(|| cancellation.is_cancelled())
            .map_err(|error| match error {
                auth::CallbackBindError::Cancelled => cancelled(),
                auth::CallbackBindError::Unavailable => ServiceError::new(
                    ErrorCode::OAuthCallbackFailed,
                    "OAuth callback listener is unavailable",
                ),
            })?;
        let pending = auth::begin_with_resource(
            prepared.config(),
            callback.redirect_uri(),
            Some(&state.oauth_resource),
        )
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::AuthenticationFailed,
                "OAuth authorization preparation failed",
            )
        })?;
        events.callback_bound(connector_contract::SensitiveInput::from(
            pending.authorize_url.clone(),
        ))?;
        let callback_params = callback
            .wait_for_callback_with_cancel(self.callback_timeout, pending.state(), || {
                cancellation.is_cancelled()
            })
            .map_err(|_| {
                if cancellation.is_cancelled() {
                    cancelled()
                } else {
                    ServiceError::new(
                        ErrorCode::OAuthCallbackFailed,
                        "OAuth callback did not complete",
                    )
                }
            })?;
        let token = run_blocking_with_cancel_checks(&cancellation, || {
            auth::complete_with_timeout(pending, callback_params, self.request_timeout)
        })?
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::AuthenticationFailed,
                "OAuth token exchange failed",
            )
        })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        let expires_at_secs = token
            .expires_in_secs
            .and_then(|seconds| now.checked_add(seconds));
        let provider_workspace_id = token.provider_workspace_id.or(provider_workspace_id);
        let workspace_domain = workspace_target
            .as_ref()
            .map(|target| target.domain().to_owned())
            .or(workspace_hint);
        let stored_metadata = StoredOAuthMetadata::new(
            StoredOAuthMetadataDraft {
                server_id: server_id.as_str().to_owned(),
                server_url: server_url.to_owned(),
                issuer: state.provider.issuer,
                authorization_endpoint: state.provider.authorization_endpoint,
                token_endpoint: state.provider.token_endpoint,
                oauth_resource: state.oauth_resource,
                client_id,
                token_endpoint_auth_method: auth_method,
                manual_client,
                provider_workspace_id,
                workspace_domain: workspace_domain.clone(),
                scopes: state.scopes,
                expires_at_secs,
            },
            StoredOAuthMetadataLimits::PRODUCTION,
        )
        .map_err(|_| {
            ServiceError::new(
                ErrorCode::AuthenticationFailed,
                "OAuth metadata validation failed",
            )
        })?;
        let masked_hint = Some(masked_secret_hint(&token.access_token));
        Ok(OAuthAuthorizeOutput::Completed(Box::new(OAuthCompletion {
            credential: OAuthCredentialUpdate {
                logical_id,
                label: state.server.name,
                bundle: secret::SecretBundle::new(
                    token.access_token,
                    token.refresh_token,
                    stored_dcr_secret,
                ),
                metadata: stored_metadata,
                masked_hint,
            },
            workspace_label: workspace_domain,
            can_choose_workspace: state.choose_workspace,
        })))
    }

    fn cancel(&self, _operation_id: &OperationId) {
        // The coordinator cancels the shared token before calling this hook. Discovery, callback,
        // and Slack primitives observe it during their work. Blocking DCR/token HTTP checks it
        // immediately before and after the request; an already in-flight synchronous request is
        // not detached or retried and therefore retains the slot for at most `request_timeout`.
    }
}

fn run_blocking_with_cancel_checks<T, E>(
    cancellation: &CancellationToken,
    run: impl FnOnce() -> Result<T, E>,
) -> Result<Result<T, E>, ServiceError> {
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let result = run();
    if cancellation.is_cancelled() {
        return Err(cancelled());
    }
    Ok(result)
}

fn provider_from_stored(metadata: &StoredOAuthMetadata) -> AuthorizationServerMetadata {
    AuthorizationServerMetadata {
        issuer: metadata.issuer().to_owned(),
        authorization_endpoint: metadata.authorization_endpoint().to_owned(),
        token_endpoint: metadata.token_endpoint().to_owned(),
        registration_endpoint: None,
        grant_types_supported: Some(vec![
            "authorization_code".to_owned(),
            "refresh_token".to_owned(),
        ]),
        scopes_supported: Some(metadata.scopes().to_vec()),
        token_endpoint_auth_methods_supported: Some(vec![
            match metadata.token_endpoint_auth_method() {
                TokenEndpointAuthMethod::None => "none".to_owned(),
                TokenEndpointAuthMethod::ClientSecretPost => "client_secret_post".to_owned(),
            },
        ]),
        code_challenge_methods_supported: Some(vec!["S256".to_owned()]),
    }
}

fn checked_add(total: usize, additional: usize) -> Result<usize, ServiceError> {
    total.checked_add(additional).ok_or_else(limit_error)
}

fn checked_mul(left: usize, right: usize) -> Result<usize, ServiceError> {
    left.checked_mul(right).ok_or_else(limit_error)
}

fn enforce_continuation_limit(bytes: usize) -> Result<(), ServiceError> {
    if bytes > StoredOAuthMetadataLimits::PRODUCTION.json_bytes {
        return Err(ServiceError::new(
            ErrorCode::LimitExceeded,
            "OAuth continuation exceeds its byte limit",
        ));
    }
    Ok(())
}

fn limit_error() -> ServiceError {
    ServiceError::new(
        ErrorCode::LimitExceeded,
        "OAuth continuation exceeds its resource limit",
    )
}

fn masked_secret_hint(secret: &secret::SecretString) -> String {
    let suffix = secret
        .expose()
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("****{suffix}")
}

fn persisted_manual_client(declared_manual: bool, metadata: Option<&StoredOAuthMetadata>) -> bool {
    metadata.map_or(declared_manual, StoredOAuthMetadata::manual_client)
}

fn split_client_secret(
    secret: Option<connector_contract::SensitiveInput>,
) -> Result<(Option<secret::SecretString>, Option<secret::SecretString>), ServiceError> {
    let Some(secret) = secret else {
        return Ok((None, None));
    };
    let secret = sensitive_to_secret(secret)?;
    let flow = secret::SecretString::new(secret.expose().to_owned());
    Ok((Some(flow), Some(secret)))
}

fn sensitive_to_secret(
    input: connector_contract::SensitiveInput,
) -> Result<secret::SecretString, ServiceError> {
    String::from_utf8(input.into_bytes())
        .map(secret::SecretString::new)
        .map_err(|error| {
            let mut bytes = error.into_bytes();
            for byte in &mut bytes {
                // SAFETY: `bytes` is an exclusively owned buffer that is dropped immediately.
                unsafe { std::ptr::write_volatile(byte, 0) };
            }
            ServiceError::new(ErrorCode::InvalidInput, "OAuth client secret is not UTF-8")
        })
}

fn secret_to_sensitive(secret: secret::SecretString) -> connector_contract::SensitiveInput {
    connector_contract::SensitiveInput::from(secret.expose().to_owned())
}

fn oauth_logical_id(
    server_id: &connector_contract::ServerId,
) -> Result<secret::LogicalCredentialId, ServiceError> {
    secret::LogicalCredentialId::new(format!("connector-oauth-{}", server_id.as_str())).map_err(
        |_| {
            ServiceError::new(
                ErrorCode::LimitExceeded,
                "connector identifier is too long for OAuth credential storage",
            )
        },
    )
}

fn invalid_stored_binding() -> ServiceError {
    ServiceError::new(
        ErrorCode::StaleResult,
        "stored OAuth client does not match the connector",
    )
}

fn cancelled() -> ServiceError {
    ServiceError::new(ErrorCode::Cancelled, "OAuth operation cancelled")
}

fn map_primitive_error(error: auth::McpOAuthPrimitiveError) -> ServiceError {
    let code = match error {
        auth::McpOAuthPrimitiveError::Cancelled => ErrorCode::Cancelled,
        auth::McpOAuthPrimitiveError::LimitExceeded => ErrorCode::LimitExceeded,
        auth::McpOAuthPrimitiveError::InvalidInput
        | auth::McpOAuthPrimitiveError::InvalidEndpoint => ErrorCode::InvalidInput,
        auth::McpOAuthPrimitiveError::DiscoveryFailed
        | auth::McpOAuthPrimitiveError::UnsupportedAuthMethod => ErrorCode::AuthenticationFailed,
    };
    ServiceError::new(code, "OAuth provider operation failed")
}

fn failed(error_code: ErrorCode) -> OAuthAuthorizeOutput {
    OAuthAuthorizeOutput::Failed(OAuthFailure {
        error_code,
        recovery: vec![OAuthRecoveryTarget {
            kind: connector_contract::SlackRecoveryKind::RetryAuthorization,
            url: None,
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn high_capacity(value: &str) -> String {
        let mut string = String::with_capacity(1024 * 1024);
        string.push_str(value);
        string
    }

    #[test]
    fn blocking_oauth_calls_check_cancellation_before_and_after_without_retry() {
        let calls = AtomicUsize::new(0);
        let cancelled_before = CancellationToken::default();
        cancelled_before.cancel();
        let error = run_blocking_with_cancel_checks(&cancelled_before, || {
            calls.fetch_add(1, Ordering::AcqRel);
            Ok::<_, ()>(())
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(calls.load(Ordering::Acquire), 0);

        let cancelled_during = CancellationToken::default();
        let result = run_blocking_with_cancel_checks(&cancelled_during, || {
            calls.fetch_add(1, Ordering::AcqRel);
            cancelled_during.cancel();
            Ok::<_, ()>(())
        });
        assert_eq!(result.unwrap_err().code, ErrorCode::Cancelled);
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn production_adapter_rejects_stdio_before_network_or_host_action() {
        let adapter = ProductionConnectorOAuth::new(
            Duration::from_secs(1),
            Duration::from_secs(1),
            "2025-06-18",
        )
        .unwrap();
        let error = adapter
            .discover(
                &OperationId::new("oauth-stdio"),
                ServerDraft {
                    id: Some(connector_contract::ServerId::new("stdio")),
                    name: "stdio".to_owned(),
                    transport: TransportDraft::Stdio {
                        command: "server".to_owned(),
                        args: Vec::new(),
                        plain_env: Vec::new(),
                        secret_env: Vec::new(),
                        inherit_env: false,
                    },
                    enabled: true,
                },
                false,
                None,
                CancellationToken::default(),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn production_continuation_debug_never_exposes_provider_state() {
        let continuation = OAuthContinuation::typed("provider-secret-state".to_owned(), 21);
        let debug = format!("{continuation:?}");
        assert_eq!(debug, "OAuthContinuation(REDACTED)");
        assert!(!debug.contains("provider-secret-state"));
    }

    #[test]
    fn continuation_budget_counts_capacity_and_compaction_reclaims_spare_allocations() {
        let mut scopes = Vec::with_capacity(4_096);
        scopes.push(high_capacity("tools.read"));
        let state = ProductionContinuation {
            server: ServerDraft {
                id: Some(connector_contract::ServerId::new("server-1")),
                name: high_capacity("server"),
                transport: TransportDraft::Http {
                    url: high_capacity("https://mcp.example.test/mcp"),
                },
                enabled: true,
            },
            provider: AuthorizationServerMetadata {
                issuer: high_capacity("https://auth.example.test/"),
                authorization_endpoint: high_capacity("https://auth.example.test/authorize"),
                token_endpoint: high_capacity("https://auth.example.test/token"),
                registration_endpoint: None,
                grant_types_supported: None,
                scopes_supported: None,
                token_endpoint_auth_methods_supported: None,
                code_challenge_methods_supported: None,
            },
            scopes,
            oauth_resource: high_capacity("https://mcp.example.test/mcp"),
            choose_workspace: false,
            stored_client: None,
        };

        assert_eq!(
            state.retained_bytes().unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        let compact = state.compact().expect("compact bounded continuation");
        let state_bytes = compact.retained_bytes().unwrap();
        assert!(state_bytes < StoredOAuthMetadataLimits::PRODUCTION.json_bytes);
        assert!(compact.server.name.capacity() < 128);
        assert!(compact.scopes.capacity() < 8);
        assert!(compact.scopes[0].capacity() < 128);

        let authority = compact_string("auth.example.test".to_owned());
        let resource = compact_string("https://mcp.example.test/mcp".to_owned());
        let display_scopes = compact_string_vec(vec!["tools.read".to_owned()]);
        let combined = discovery_retained_bytes(
            &compact,
            authority.capacity(),
            resource.capacity(),
            &display_scopes,
            display_scopes.capacity(),
        )
        .unwrap();
        assert_eq!(
            combined,
            state_bytes
                + authority.capacity()
                + resource.capacity()
                + display_scopes.capacity() * std::mem::size_of::<String>()
                + display_scopes.iter().map(String::capacity).sum::<usize>()
                + 2 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn dcr_origin_is_not_persisted_as_manual_and_hint_does_not_clone_full_token() {
        assert!(!persisted_manual_client(false, None));
        assert!(persisted_manual_client(true, None));
        let token = secret::SecretString::new("very-long-access-token-1234".to_owned());
        let hint = masked_secret_hint(&token);
        assert_eq!(hint, "****1234");
        assert!(!hint.contains("very-long-access-token"));
    }
}
