//! Connector-neutral MCP OAuth discovery and provider configuration.
//!
//! This module composes the RFC 9728/8414 primitives but has no dependency on MCP transports,
//! storage, UI, or an async runtime.

use std::time::Duration;

use secret::SecretString;

use crate::{
    AuthorizationServerMetadata, DiscoveryHeaders, OAuthProviderConfig, SLACK_MCP_URL,
    SLACK_OAUTH_RESOURCE, StoredOAuthMetadataLimits, TokenEndpointAuthMethod,
    discover_authorization_server, discover_protected_resource, is_valid_slack_team_id,
    validate_https_or_loopback,
};

#[derive(Clone, PartialEq, Eq)]
pub struct McpOAuthChallenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

impl McpOAuthChallenge {
    pub fn new(resource_metadata: Option<String>, scope: Option<String>) -> Self {
        Self {
            resource_metadata,
            scope,
        }
    }

    pub fn resource_metadata(&self) -> Option<&str> {
        self.resource_metadata.as_deref()
    }

    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }
}

impl std::fmt::Debug for McpOAuthChallenge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthChallenge")
            .field("has_resource_metadata", &self.resource_metadata.is_some())
            .field("has_scope", &self.scope.is_some())
            .finish()
    }
}

pub struct McpOAuthDiscovery {
    metadata: AuthorizationServerMetadata,
    scopes: Vec<String>,
    oauth_resource: String,
}

impl McpOAuthDiscovery {
    pub fn metadata(&self) -> &AuthorizationServerMetadata {
        &self.metadata
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn oauth_resource(&self) -> &str {
        &self.oauth_resource
    }

    pub fn into_parts(self) -> (AuthorizationServerMetadata, Vec<String>, String) {
        (self.metadata, self.scopes, self.oauth_resource)
    }
}

impl std::fmt::Debug for McpOAuthDiscovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthDiscovery")
            .field("endpoints", &"REDACTED")
            .field("scope_count", &self.scopes.len())
            .finish()
    }
}

pub struct PreparedOAuthProviderConfig {
    config: OAuthProviderConfig,
    auth_method: TokenEndpointAuthMethod,
}

impl PreparedOAuthProviderConfig {
    pub fn config(&self) -> &OAuthProviderConfig {
        &self.config
    }

    pub fn into_config(self) -> OAuthProviderConfig {
        self.config
    }

    pub fn auth_method(&self) -> TokenEndpointAuthMethod {
        self.auth_method
    }
}

impl std::fmt::Debug for PreparedOAuthProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedOAuthProviderConfig")
            .field("configuration", &"REDACTED")
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpOAuthPrimitiveError {
    Cancelled,
    InvalidInput,
    InvalidEndpoint,
    LimitExceeded,
    DiscoveryFailed,
    UnsupportedAuthMethod,
}

impl std::fmt::Display for McpOAuthPrimitiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Cancelled => "MCP OAuth operation was cancelled",
            Self::InvalidInput => "MCP OAuth input is invalid",
            Self::InvalidEndpoint => "MCP OAuth endpoint is invalid",
            Self::LimitExceeded => "MCP OAuth input exceeds its resource limit",
            Self::DiscoveryFailed => "MCP OAuth discovery failed",
            Self::UnsupportedAuthMethod => "MCP OAuth client authentication is unsupported",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for McpOAuthPrimitiveError {}

/// Returns the validated RFC 8707 resource. Slack publishes its origin as the protected resource;
/// other providers remain bound to the exact normalized MCP endpoint.
pub fn canonical_mcp_oauth_resource(server_url: &str) -> Result<String, McpOAuthPrimitiveError> {
    let server = normalized_endpoint(server_url)?;
    let slack = normalized_endpoint(SLACK_MCP_URL)?;
    if server.trim_end_matches('/') == slack.trim_end_matches('/') {
        normalized_endpoint(SLACK_OAUTH_RESOURCE)?;
        Ok(SLACK_OAUTH_RESOURCE.to_owned())
    } else {
        Ok(server)
    }
}

/// Composes RFC 9728 protected-resource and RFC 8414 authorization-server discovery. The
/// cancellation callback is checked around every blocking network stage. PRM absence falls back to
/// the validated MCP origin for compatibility; invalid AS metadata never falls back here.
pub fn discover_mcp_oauth_cancellable(
    timeout: Duration,
    server_url: &str,
    challenge: &McpOAuthChallenge,
    protocol_version: &str,
    limits: StoredOAuthMetadataLimits,
    cancelled: impl Fn() -> bool,
) -> Result<McpOAuthDiscovery, McpOAuthPrimitiveError> {
    let limits = limits
        .validate()
        .map_err(|_| McpOAuthPrimitiveError::LimitExceeded)?;
    validate_protocol_version(protocol_version)?;
    validate_challenge(challenge, limits)?;
    if cancelled() {
        return Err(McpOAuthPrimitiveError::Cancelled);
    }
    let normalized_server = normalized_endpoint(server_url)?;
    let parsed_server = validate_https_or_loopback(&normalized_server)
        .map_err(|_| McpOAuthPrimitiveError::InvalidEndpoint)?;
    let origin = parsed_server.origin().ascii_serialization();
    let oauth_resource = canonical_mcp_oauth_resource(&normalized_server)?;
    let headers = DiscoveryHeaders::new(
        &normalized_server,
        vec![(
            "MCP-Protocol-Version".to_owned(),
            protocol_version.to_owned(),
        )],
    )
    .map_err(|_| McpOAuthPrimitiveError::InvalidInput)?;

    let protected = discover_protected_resource(
        timeout,
        &oauth_resource,
        challenge.resource_metadata(),
        Some(&headers),
    );
    if cancelled() {
        return Err(McpOAuthPrimitiveError::Cancelled);
    }
    let (authorization_server, protected_scopes) = match protected {
        Ok(metadata) => {
            let authorization_server = metadata
                .authorization_servers
                .iter()
                .find_map(|candidate| normalized_endpoint(candidate).ok())
                .unwrap_or_else(|| origin.clone());
            (authorization_server, metadata.scopes_supported)
        }
        Err(_) => (origin, None),
    };

    if cancelled() {
        return Err(McpOAuthPrimitiveError::Cancelled);
    }
    let metadata = discover_authorization_server(timeout, &authorization_server, Some(&headers))
        .map_err(|_| McpOAuthPrimitiveError::DiscoveryFailed)?;
    if cancelled() {
        return Err(McpOAuthPrimitiveError::Cancelled);
    }
    validate_authorization_metadata(&metadata)?;

    let scopes = if let Some(raw) = challenge.scope() {
        let parsed = parse_scopes_bounded(raw, limits)?;
        if parsed.is_empty() {
            validate_scope_vec(protected_scopes.unwrap_or_default(), limits)?
        } else {
            parsed
        }
    } else {
        validate_scope_vec(protected_scopes.unwrap_or_default(), limits)?
    };
    Ok(McpOAuthDiscovery {
        metadata,
        scopes,
        oauth_resource,
    })
}

pub fn parse_scopes_bounded(
    raw: &str,
    limits: StoredOAuthMetadataLimits,
) -> Result<Vec<String>, McpOAuthPrimitiveError> {
    let limits = limits
        .validate()
        .map_err(|_| McpOAuthPrimitiveError::LimitExceeded)?;
    if raw.len() > limits.scope_bytes {
        return Err(McpOAuthPrimitiveError::LimitExceeded);
    }
    let mut scopes = Vec::with_capacity(limits.scopes.min(8));
    let mut bytes = 0usize;
    for scope in raw.split_whitespace() {
        if scopes.len() >= limits.scopes || scope.len() > limits.field_bytes {
            return Err(McpOAuthPrimitiveError::LimitExceeded);
        }
        if scope.is_empty() || scope.contains('\0') {
            return Err(McpOAuthPrimitiveError::InvalidInput);
        }
        bytes = bytes
            .checked_add(scope.len())
            .ok_or(McpOAuthPrimitiveError::LimitExceeded)?;
        if bytes > limits.scope_bytes {
            return Err(McpOAuthPrimitiveError::LimitExceeded);
        }
        scopes.push(scope.to_owned());
    }
    Ok(scopes)
}

/// Selects only a transport method this application implements. A confidential secret is never
/// silently sent with Basic auth, and a server requiring a secret cannot be used as a public client.
pub fn select_token_endpoint_auth_method(
    metadata: &AuthorizationServerMetadata,
    has_client_secret: bool,
) -> Result<TokenEndpointAuthMethod, McpOAuthPrimitiveError> {
    let supported = metadata
        .token_endpoint_auth_methods_supported
        .as_deref()
        .unwrap_or(&[]);
    if has_client_secret {
        if supported
            .iter()
            .any(|method| method == "client_secret_post")
        {
            return Ok(TokenEndpointAuthMethod::ClientSecretPost);
        }
        return Err(McpOAuthPrimitiveError::UnsupportedAuthMethod);
    }
    if supported.is_empty() || supported.iter().any(|method| method == "none") {
        Ok(TokenEndpointAuthMethod::None)
    } else {
        Err(McpOAuthPrimitiveError::UnsupportedAuthMethod)
    }
}

/// Builds a provider flow configuration while consuming (never cloning) the optional client
/// secret. Slack's optional team hint is validated before it becomes an authorize parameter.
pub fn prepare_oauth_provider_config(
    metadata: &AuthorizationServerMetadata,
    client_id: String,
    client_secret: Option<SecretString>,
    scopes: Vec<String>,
    slack_team_id: Option<String>,
    limits: StoredOAuthMetadataLimits,
) -> Result<PreparedOAuthProviderConfig, McpOAuthPrimitiveError> {
    let limits = limits
        .validate()
        .map_err(|_| McpOAuthPrimitiveError::LimitExceeded)?;
    validate_authorization_metadata(metadata)?;
    if client_id.is_empty() || client_id.len() > limits.field_bytes || client_id.contains('\0') {
        return Err(McpOAuthPrimitiveError::InvalidInput);
    }
    let scopes = validate_scope_vec(scopes, limits)?;
    let auth_method =
        select_token_endpoint_auth_method(metadata, client_secret.as_ref().is_some())?;
    let extra_authorize_params = match slack_team_id {
        Some(team_id) if is_valid_slack_team_id(&team_id) => {
            vec![("team".to_owned(), team_id)]
        }
        Some(_) => return Err(McpOAuthPrimitiveError::InvalidInput),
        None => Vec::new(),
    };
    Ok(PreparedOAuthProviderConfig {
        config: OAuthProviderConfig {
            auth_url: metadata.authorization_endpoint.clone(),
            token_url: metadata.token_endpoint.clone(),
            client_id,
            client_secret,
            client_secret_post: auth_method == TokenEndpointAuthMethod::ClientSecretPost,
            scopes,
            extra_authorize_params,
        },
        auth_method,
    })
}

/// Validated display-only authority. The caller may put the returned origin in a UI snapshot but
/// should still avoid logging it.
pub fn oauth_authority_display(url: &str) -> Result<String, McpOAuthPrimitiveError> {
    let normalized = normalized_endpoint(url)?;
    validate_https_or_loopback(&normalized)
        .map(|parsed| parsed.origin().ascii_serialization())
        .map_err(|_| McpOAuthPrimitiveError::InvalidEndpoint)
}

fn normalized_endpoint(value: &str) -> Result<String, McpOAuthPrimitiveError> {
    let value = value.trim();
    if value.is_empty() || value.len() > StoredOAuthMetadataLimits::PRODUCTION.field_bytes {
        return Err(McpOAuthPrimitiveError::InvalidEndpoint);
    }
    let parsed =
        validate_https_or_loopback(value).map_err(|_| McpOAuthPrimitiveError::InvalidEndpoint)?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(McpOAuthPrimitiveError::InvalidEndpoint);
    }
    Ok(parsed.to_string())
}

fn validate_protocol_version(value: &str) -> Result<(), McpOAuthPrimitiveError> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\0')
    {
        return Err(McpOAuthPrimitiveError::InvalidInput);
    }
    Ok(())
}

fn validate_challenge(
    challenge: &McpOAuthChallenge,
    limits: StoredOAuthMetadataLimits,
) -> Result<(), McpOAuthPrimitiveError> {
    if challenge
        .resource_metadata()
        .is_some_and(|value| value.len() > limits.field_bytes || value.contains('\0'))
        || challenge
            .scope()
            .is_some_and(|value| value.len() > limits.scope_bytes || value.contains('\0'))
    {
        return Err(McpOAuthPrimitiveError::LimitExceeded);
    }
    Ok(())
}

fn validate_authorization_metadata(
    metadata: &AuthorizationServerMetadata,
) -> Result<(), McpOAuthPrimitiveError> {
    for endpoint in [
        metadata.issuer.as_str(),
        metadata.authorization_endpoint.as_str(),
        metadata.token_endpoint.as_str(),
    ] {
        normalized_endpoint(endpoint)?;
    }
    if let Some(endpoint) = metadata.registration_endpoint.as_deref() {
        normalized_endpoint(endpoint)?;
    }
    Ok(())
}

fn validate_scope_vec(
    scopes: Vec<String>,
    limits: StoredOAuthMetadataLimits,
) -> Result<Vec<String>, McpOAuthPrimitiveError> {
    if scopes.len() > limits.scopes {
        return Err(McpOAuthPrimitiveError::LimitExceeded);
    }
    let mut bytes = 0usize;
    for scope in &scopes {
        if scope.is_empty()
            || scope.len() > limits.field_bytes
            || scope.contains('\0')
            || scope.chars().any(char::is_whitespace)
        {
            return Err(McpOAuthPrimitiveError::InvalidInput);
        }
        bytes = bytes
            .checked_add(scope.len())
            .ok_or(McpOAuthPrimitiveError::LimitExceeded)?;
        if bytes > limits.scope_bytes {
            return Err(McpOAuthPrimitiveError::LimitExceeded);
        }
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(methods: Option<Vec<&str>>) -> AuthorizationServerMetadata {
        AuthorizationServerMetadata {
            issuer: "https://auth.example.test/".to_owned(),
            authorization_endpoint: "https://auth.example.test/authorize".to_owned(),
            token_endpoint: "https://auth.example.test/token".to_owned(),
            registration_endpoint: Some("https://auth.example.test/register".to_owned()),
            grant_types_supported: None,
            scopes_supported: None,
            token_endpoint_auth_methods_supported: methods
                .map(|values| values.into_iter().map(str::to_owned).collect()),
            code_challenge_methods_supported: None,
        }
    }

    #[test]
    fn slack_resource_is_origin_and_generic_resource_is_exact_endpoint() {
        assert_eq!(
            canonical_mcp_oauth_resource(SLACK_MCP_URL).unwrap(),
            SLACK_OAUTH_RESOURCE
        );
        assert_eq!(
            canonical_mcp_oauth_resource("https://mcp.slack.com/mcp/").unwrap(),
            SLACK_OAUTH_RESOURCE
        );
        assert_eq!(
            canonical_mcp_oauth_resource("https://example.test/mcp").unwrap(),
            "https://example.test/mcp"
        );
    }

    #[test]
    fn auth_method_selection_rejects_basic_and_unknown_only() {
        assert_eq!(
            select_token_endpoint_auth_method(&metadata(None), false).unwrap(),
            TokenEndpointAuthMethod::None
        );
        assert_eq!(
            select_token_endpoint_auth_method(
                &metadata(Some(vec!["client_secret_basic", "client_secret_post"])),
                true,
            )
            .unwrap(),
            TokenEndpointAuthMethod::ClientSecretPost
        );
        for methods in [vec!["client_secret_basic"], vec!["vendor_auth"]] {
            assert_eq!(
                select_token_endpoint_auth_method(&metadata(Some(methods)), true).unwrap_err(),
                McpOAuthPrimitiveError::UnsupportedAuthMethod
            );
        }
    }

    #[test]
    fn scope_parser_accepts_exact_count_and_rejects_plus_one() {
        let limits = StoredOAuthMetadataLimits {
            scopes: 2,
            ..StoredOAuthMetadataLimits::PRODUCTION
        };
        assert_eq!(
            parse_scopes_bounded("read write", limits).unwrap(),
            ["read", "write"]
        );
        assert_eq!(
            parse_scopes_bounded("read write admin", limits).unwrap_err(),
            McpOAuthPrimitiveError::LimitExceeded
        );
    }

    #[test]
    fn provider_config_consumes_secret_and_rejects_invalid_team() {
        let prepared = prepare_oauth_provider_config(
            &metadata(Some(vec!["client_secret_post"])),
            "client".to_owned(),
            Some(SecretString::new("client-secret-value".to_owned())),
            vec!["read".to_owned()],
            Some("T12345678".to_owned()),
            StoredOAuthMetadataLimits::PRODUCTION,
        )
        .unwrap();
        assert_eq!(
            prepared.auth_method(),
            TokenEndpointAuthMethod::ClientSecretPost
        );
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("client-secret-value"));
        assert!(!debug.contains("auth.example.test"));
        assert!(
            prepare_oauth_provider_config(
                &metadata(None),
                "client".to_owned(),
                None,
                vec![],
                Some("wrong".to_owned()),
                StoredOAuthMetadataLimits::PRODUCTION,
            )
            .is_err()
        );
    }

    #[test]
    fn discovery_checks_cancellation_before_network() {
        let result = discover_mcp_oauth_cancellable(
            Duration::from_secs(1),
            "https://mcp.example.test/mcp",
            &McpOAuthChallenge::new(None, None),
            "2025-06-18",
            StoredOAuthMetadataLimits::PRODUCTION,
            || true,
        );
        assert_eq!(result.unwrap_err(), McpOAuthPrimitiveError::Cancelled);
    }

    #[test]
    fn discovery_and_challenge_debug_hide_raw_values() {
        let challenge = McpOAuthChallenge::new(
            Some("https://secret.example.test/meta".to_owned()),
            Some("sensitive.scope".to_owned()),
        );
        let debug = format!("{challenge:?}");
        assert!(!debug.contains("secret.example.test"));
        assert!(!debug.contains("sensitive.scope"));
    }
}
