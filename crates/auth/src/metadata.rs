//! Typed, bounded durable OAuth metadata.
//!
//! Token material never belongs in this model. Callers persist it through
//! `secret::SecretBundle`; this module only stores the non-secret binding needed to validate and
//! refresh that bundle.

use serde::{Deserialize, Serialize};

use crate::{
    SLACK_MCP_URL, is_valid_slack_team_id, normalize_slack_workspace_domain,
    validate_https_or_loopback,
};

/// Fixed production ceilings for one durable OAuth metadata document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredOAuthMetadataLimits {
    pub json_bytes: usize,
    pub field_bytes: usize,
    pub scopes: usize,
    pub scope_bytes: usize,
    pub workspace_bytes: usize,
}

impl StoredOAuthMetadataLimits {
    pub const PRODUCTION: Self = Self {
        json_bytes: 64 * 1024,
        field_bytes: 16 * 1024,
        scopes: 256,
        scope_bytes: 16 * 1024,
        workspace_bytes: 256,
    };

    pub(crate) fn validate(self) -> Result<Self, StoredOAuthMetadataError> {
        let ceiling = Self::PRODUCTION;
        let fields = [
            (self.json_bytes, ceiling.json_bytes),
            (self.field_bytes, ceiling.field_bytes),
            (self.scopes, ceiling.scopes),
            (self.scope_bytes, ceiling.scope_bytes),
            (self.workspace_bytes, ceiling.workspace_bytes),
        ];
        if fields
            .into_iter()
            .any(|(value, maximum)| value == 0 || value > maximum)
        {
            return Err(StoredOAuthMetadataError::InvalidLimits);
        }
        Ok(self)
    }
}

impl Default for StoredOAuthMetadataLimits {
    fn default() -> Self {
        Self::PRODUCTION
    }
}

/// Supported token endpoint authentication. Basic and vendor-specific methods are deliberately
/// absent: silently falling back to them would change where a client secret is transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenEndpointAuthMethod {
    None,
    ClientSecretPost,
}

impl TokenEndpointAuthMethod {
    pub fn uses_client_secret(self) -> bool {
        matches!(self, Self::ClientSecretPost)
    }
}

/// Construction input for [`StoredOAuthMetadata`]. It is non-secret but intentionally has no
/// `Debug` implementation because its URLs and provider identifiers must not enter diagnostics.
pub struct StoredOAuthMetadataDraft {
    pub server_id: String,
    pub server_url: String,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub oauth_resource: String,
    pub client_id: String,
    pub token_endpoint_auth_method: TokenEndpointAuthMethod,
    pub manual_client: bool,
    pub provider_workspace_id: Option<String>,
    pub workspace_domain: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at_secs: Option<u64>,
}

/// Validated non-secret OAuth binding stored alongside a logical credential pointer.
///
/// This type is cloneable because it contains no token, code, verifier, or client secret. Its
/// custom `Debug` output nevertheless hides raw URLs and provider identifiers.
#[derive(Clone, PartialEq, Eq)]
pub struct StoredOAuthMetadata {
    server_id: String,
    server_url: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    oauth_resource: String,
    client_id: String,
    token_endpoint_auth_method: TokenEndpointAuthMethod,
    manual_client: bool,
    provider_workspace_id: Option<String>,
    workspace_domain: Option<String>,
    scopes: Vec<String>,
    expires_at_secs: Option<u64>,
}

impl StoredOAuthMetadata {
    pub fn new(
        draft: StoredOAuthMetadataDraft,
        limits: StoredOAuthMetadataLimits,
    ) -> Result<Self, StoredOAuthMetadataError> {
        let limits = limits.validate()?;
        validate_identifier(&draft.server_id, limits.field_bytes)?;
        validate_identifier(&draft.client_id, limits.field_bytes)?;

        let server_url = normalize_endpoint(&draft.server_url, limits.field_bytes)?;
        let issuer = normalize_endpoint(&draft.issuer, limits.field_bytes)?;
        let authorization_endpoint =
            normalize_endpoint(&draft.authorization_endpoint, limits.field_bytes)?;
        let token_endpoint = normalize_endpoint(&draft.token_endpoint, limits.field_bytes)?;
        let oauth_resource = normalize_oauth_resource(&draft.oauth_resource, limits.field_bytes)?;
        validate_scopes(&draft.scopes, limits)?;

        let is_slack = is_slack_server_url(&server_url, limits.field_bytes)?;
        let provider_workspace_id = validate_workspace_id(
            draft.provider_workspace_id,
            is_slack,
            limits.workspace_bytes,
        )?;
        let workspace_domain =
            validate_workspace_domain(draft.workspace_domain, is_slack, limits.workspace_bytes)?;

        Ok(Self {
            server_id: draft.server_id,
            server_url,
            issuer,
            authorization_endpoint,
            token_endpoint,
            oauth_resource,
            client_id: draft.client_id,
            token_endpoint_auth_method: draft.token_endpoint_auth_method,
            manual_client: draft.manual_client,
            provider_workspace_id,
            workspace_domain,
            scopes: draft.scopes,
            expires_at_secs: draft.expires_at_secs,
        })
    }

    /// Parses legacy or current JSON only after enforcing the caller's byte ceiling, then validates
    /// the exact server binding and every endpoint before returning a typed value.
    pub fn from_json_bounded(
        json: &[u8],
        expected_server_id: &str,
        exact_server_url: &str,
        limits: StoredOAuthMetadataLimits,
    ) -> Result<Self, StoredOAuthMetadataError> {
        let limits = limits.validate()?;
        if json.len() > limits.json_bytes {
            return Err(StoredOAuthMetadataError::JsonLimitExceeded);
        }
        let wire: StoredOAuthMetadataWire =
            serde_json::from_slice(json).map_err(|_| StoredOAuthMetadataError::InvalidJson)?;
        let auth_method = wire.auth_method()?;
        let metadata = Self::new(
            StoredOAuthMetadataDraft {
                server_id: wire.server_id,
                server_url: wire.server_url,
                issuer: wire.issuer,
                authorization_endpoint: wire.authorization_endpoint,
                token_endpoint: wire.token_endpoint,
                oauth_resource: wire
                    .oauth_resource
                    .unwrap_or_else(|| exact_server_url.to_owned()),
                client_id: wire.client_id,
                token_endpoint_auth_method: auth_method,
                manual_client: wire.manual_client,
                provider_workspace_id: wire.provider_workspace_id,
                workspace_domain: wire.workspace_domain,
                scopes: wire.scopes,
                expires_at_secs: wire.expires_at_secs,
            },
            limits,
        )?;
        metadata.validate_for_binding(expected_server_id, exact_server_url, limits)?;
        Ok(metadata)
    }

    /// Serializes only after revalidating the binding and rejects an output one byte beyond the
    /// fixed ceiling.
    pub fn to_json_bounded(
        &self,
        expected_server_id: &str,
        exact_server_url: &str,
        limits: StoredOAuthMetadataLimits,
    ) -> Result<String, StoredOAuthMetadataError> {
        let limits = limits.validate()?;
        self.validate_for_binding(expected_server_id, exact_server_url, limits)?;
        let wire = StoredOAuthMetadataSerializeWire {
            server_id: &self.server_id,
            server_url: &self.server_url,
            issuer: &self.issuer,
            authorization_endpoint: &self.authorization_endpoint,
            token_endpoint: &self.token_endpoint,
            oauth_resource: &self.oauth_resource,
            client_id: &self.client_id,
            token_endpoint_auth_method: self.token_endpoint_auth_method,
            manual_client: self.manual_client,
            provider_workspace_id: self.provider_workspace_id.as_deref(),
            workspace_domain: self.workspace_domain.as_deref(),
            scopes: &self.scopes,
            expires_at_secs: self.expires_at_secs,
        };
        let json = serde_json::to_string(&wire)
            .map_err(|_| StoredOAuthMetadataError::SerializationFailed)?;
        if json.len() > limits.json_bytes {
            return Err(StoredOAuthMetadataError::JsonLimitExceeded);
        }
        Ok(json)
    }

    pub fn validate_for_binding(
        &self,
        expected_server_id: &str,
        exact_server_url: &str,
        limits: StoredOAuthMetadataLimits,
    ) -> Result<(), StoredOAuthMetadataError> {
        let limits = limits.validate()?;
        if self.server_id != expected_server_id {
            return Err(StoredOAuthMetadataError::ServerMismatch);
        }
        let expected_url = normalize_endpoint(exact_server_url, limits.field_bytes)?;
        if self.server_url != expected_url {
            return Err(StoredOAuthMetadataError::UrlMismatch);
        }
        // Re-run the complete validation so a value created by an older binary cannot bypass new
        // endpoint or resource ceilings after deserialization.
        validate_identifier(&self.server_id, limits.field_bytes)?;
        validate_identifier(&self.client_id, limits.field_bytes)?;
        for endpoint in [
            &self.server_url,
            &self.issuer,
            &self.authorization_endpoint,
            &self.token_endpoint,
        ] {
            let normalized = normalize_endpoint(endpoint, limits.field_bytes)?;
            if normalized != *endpoint {
                return Err(StoredOAuthMetadataError::InvalidEndpoint);
            }
        }
        if normalize_oauth_resource(&self.oauth_resource, limits.field_bytes)?
            != self.oauth_resource
        {
            return Err(StoredOAuthMetadataError::InvalidEndpoint);
        }
        validate_scopes(&self.scopes, limits)?;
        let is_slack = is_slack_server_url(&self.server_url, limits.field_bytes)?;
        validate_workspace_id(
            self.provider_workspace_id.clone(),
            is_slack,
            limits.workspace_bytes,
        )?;
        validate_workspace_domain(
            self.workspace_domain.clone(),
            is_slack,
            limits.workspace_bytes,
        )?;
        Ok(())
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn authorization_endpoint(&self) -> &str {
        &self.authorization_endpoint
    }

    pub fn token_endpoint(&self) -> &str {
        &self.token_endpoint
    }

    pub fn oauth_resource(&self) -> &str {
        &self.oauth_resource
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn token_endpoint_auth_method(&self) -> TokenEndpointAuthMethod {
        self.token_endpoint_auth_method
    }

    pub fn manual_client(&self) -> bool {
        self.manual_client
    }

    pub fn provider_workspace_id(&self) -> Option<&str> {
        self.provider_workspace_id.as_deref()
    }

    pub fn workspace_domain(&self) -> Option<&str> {
        self.workspace_domain.as_deref()
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    pub fn expires_at_secs(&self) -> Option<u64> {
        self.expires_at_secs
    }
}

impl std::fmt::Debug for StoredOAuthMetadata {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredOAuthMetadata")
            .field("binding", &"REDACTED")
            .field(
                "token_endpoint_auth_method",
                &self.token_endpoint_auth_method,
            )
            .field("manual_client", &self.manual_client)
            .field("scope_count", &self.scopes.len())
            .field("has_workspace", &self.provider_workspace_id.is_some())
            .field("has_expiry", &self.expires_at_secs.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredOAuthMetadataError {
    InvalidLimits,
    JsonLimitExceeded,
    InvalidJson,
    SerializationFailed,
    InvalidIdentifier,
    InvalidEndpoint,
    UnsupportedAuthMethod,
    ConflictingAuthMethod,
    ScopeLimitExceeded,
    InvalidScope,
    InvalidWorkspace,
    ServerMismatch,
    UrlMismatch,
}

impl std::fmt::Display for StoredOAuthMetadataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidLimits => "OAuth metadata limits are invalid",
            Self::JsonLimitExceeded => "OAuth metadata JSON exceeds its byte limit",
            Self::InvalidJson => "OAuth metadata JSON is invalid",
            Self::SerializationFailed => "OAuth metadata serialization failed",
            Self::InvalidIdentifier => "OAuth metadata identifier is invalid",
            Self::InvalidEndpoint => "OAuth metadata endpoint is invalid",
            Self::UnsupportedAuthMethod => "OAuth token endpoint auth method is unsupported",
            Self::ConflictingAuthMethod => "OAuth token endpoint auth methods conflict",
            Self::ScopeLimitExceeded => "OAuth metadata scopes exceed their resource limit",
            Self::InvalidScope => "OAuth metadata scope is invalid",
            Self::InvalidWorkspace => "OAuth workspace metadata is invalid",
            Self::ServerMismatch => "OAuth metadata server binding does not match",
            Self::UrlMismatch => "OAuth metadata URL binding does not match",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StoredOAuthMetadataError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredOAuthMetadataWire {
    server_id: String,
    server_url: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    oauth_resource: Option<String>,
    client_id: String,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
    #[serde(default)]
    client_secret_post: Option<bool>,
    #[serde(default)]
    manual_client: bool,
    #[serde(default, alias = "slack_team_id")]
    provider_workspace_id: Option<String>,
    #[serde(default, alias = "slack_workspace_domain")]
    workspace_domain: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expires_at_secs: Option<u64>,
}

impl StoredOAuthMetadataWire {
    fn auth_method(&self) -> Result<TokenEndpointAuthMethod, StoredOAuthMetadataError> {
        let current = match self.token_endpoint_auth_method.as_deref() {
            Some("none") => Some(TokenEndpointAuthMethod::None),
            Some("client_secret_post") => Some(TokenEndpointAuthMethod::ClientSecretPost),
            Some(_) => return Err(StoredOAuthMetadataError::UnsupportedAuthMethod),
            None => None,
        };
        let legacy = self.client_secret_post.map(|post| {
            if post {
                TokenEndpointAuthMethod::ClientSecretPost
            } else {
                TokenEndpointAuthMethod::None
            }
        });
        match (current, legacy) {
            (Some(current), Some(legacy)) if current != legacy => {
                Err(StoredOAuthMetadataError::ConflictingAuthMethod)
            }
            (Some(current), _) => Ok(current),
            (None, Some(legacy)) => Ok(legacy),
            // The legacy field was `#[serde(default)] bool`, so omission meant false.
            (None, None) => Ok(TokenEndpointAuthMethod::None),
        }
    }
}

#[derive(Serialize)]
struct StoredOAuthMetadataSerializeWire<'a> {
    server_id: &'a str,
    server_url: &'a str,
    issuer: &'a str,
    authorization_endpoint: &'a str,
    token_endpoint: &'a str,
    oauth_resource: &'a str,
    client_id: &'a str,
    token_endpoint_auth_method: TokenEndpointAuthMethod,
    manual_client: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_workspace_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_domain: Option<&'a str>,
    scopes: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at_secs: Option<u64>,
}

fn validate_identifier(value: &str, max_bytes: usize) -> Result<(), StoredOAuthMetadataError> {
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(StoredOAuthMetadataError::InvalidIdentifier);
    }
    Ok(())
}

fn normalize_endpoint(value: &str, max_bytes: usize) -> Result<String, StoredOAuthMetadataError> {
    let value = value.trim();
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(StoredOAuthMetadataError::InvalidEndpoint);
    }
    let parsed =
        validate_https_or_loopback(value).map_err(|_| StoredOAuthMetadataError::InvalidEndpoint)?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(StoredOAuthMetadataError::InvalidEndpoint);
    }
    Ok(parsed.to_string())
}

fn normalize_oauth_resource(
    value: &str,
    max_bytes: usize,
) -> Result<String, StoredOAuthMetadataError> {
    let normalized = normalize_endpoint(value, max_bytes)?;
    let slack_resource = normalize_endpoint(crate::SLACK_OAUTH_RESOURCE, max_bytes)?;
    if normalized == slack_resource {
        // Slack documents the RFC 8707 resource as the origin without a trailing slash. Keep that
        // exact wire value while still validating it through Url first.
        Ok(crate::SLACK_OAUTH_RESOURCE.to_owned())
    } else {
        Ok(normalized)
    }
}

fn is_slack_server_url(
    normalized_server_url: &str,
    max_bytes: usize,
) -> Result<bool, StoredOAuthMetadataError> {
    let slack = normalize_endpoint(SLACK_MCP_URL, max_bytes)?;
    Ok(normalized_server_url.trim_end_matches('/') == slack.trim_end_matches('/'))
}

fn validate_scopes(
    scopes: &[String],
    limits: StoredOAuthMetadataLimits,
) -> Result<(), StoredOAuthMetadataError> {
    if scopes.len() > limits.scopes {
        return Err(StoredOAuthMetadataError::ScopeLimitExceeded);
    }
    let mut bytes = 0usize;
    for scope in scopes {
        if scope.is_empty()
            || scope.len() > limits.field_bytes
            || scope.contains('\0')
            || scope.chars().any(char::is_whitespace)
        {
            return Err(StoredOAuthMetadataError::InvalidScope);
        }
        bytes = bytes
            .checked_add(scope.len())
            .ok_or(StoredOAuthMetadataError::ScopeLimitExceeded)?;
        if bytes > limits.scope_bytes {
            return Err(StoredOAuthMetadataError::ScopeLimitExceeded);
        }
    }
    Ok(())
}

fn validate_workspace_id(
    value: Option<String>,
    is_slack: bool,
    max_bytes: usize,
) -> Result<Option<String>, StoredOAuthMetadataError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(StoredOAuthMetadataError::InvalidWorkspace);
    }
    if is_slack && !is_valid_slack_team_id(&value) {
        return Err(StoredOAuthMetadataError::InvalidWorkspace);
    }
    Ok(Some(value))
}

fn validate_workspace_domain(
    value: Option<String>,
    is_slack: bool,
    max_bytes: usize,
) -> Result<Option<String>, StoredOAuthMetadataError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !is_slack || value.len() > max_bytes {
        return Err(StoredOAuthMetadataError::InvalidWorkspace);
    }
    let normalized = normalize_slack_workspace_domain(&value)
        .map_err(|_| StoredOAuthMetadataError::InvalidWorkspace)?;
    if normalized != value.trim().to_ascii_lowercase() {
        return Err(StoredOAuthMetadataError::InvalidWorkspace);
    }
    Ok(Some(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> StoredOAuthMetadataDraft {
        StoredOAuthMetadataDraft {
            server_id: "server-1".to_owned(),
            server_url: "https://mcp.example.test/mcp".to_owned(),
            issuer: "https://auth.example.test/".to_owned(),
            authorization_endpoint: "https://auth.example.test/authorize".to_owned(),
            token_endpoint: "https://auth.example.test/token".to_owned(),
            oauth_resource: "https://mcp.example.test/mcp".to_owned(),
            client_id: "desktop-client".to_owned(),
            token_endpoint_auth_method: TokenEndpointAuthMethod::None,
            manual_client: false,
            provider_workspace_id: None,
            workspace_domain: None,
            scopes: vec!["tools.read".to_owned()],
            expires_at_secs: Some(42),
        }
    }

    #[test]
    fn legacy_bool_json_decodes_and_new_json_uses_typed_method() {
        let legacy = br#"{
            "server_id":"server-1",
            "server_url":"https://mcp.example.test/mcp",
            "issuer":"https://auth.example.test/",
            "authorization_endpoint":"https://auth.example.test/authorize",
            "token_endpoint":"https://auth.example.test/token",
            "client_id":"desktop-client",
            "client_secret_post":true,
            "scopes":["tools.read"]
        }"#;
        let metadata = StoredOAuthMetadata::from_json_bounded(
            legacy,
            "server-1",
            "https://mcp.example.test/mcp",
            StoredOAuthMetadataLimits::PRODUCTION,
        )
        .unwrap();
        assert_eq!(
            metadata.token_endpoint_auth_method(),
            TokenEndpointAuthMethod::ClientSecretPost
        );
        let json = metadata
            .to_json_bounded(
                "server-1",
                "https://mcp.example.test/mcp",
                StoredOAuthMetadataLimits::PRODUCTION,
            )
            .unwrap();
        assert!(json.contains("\"token_endpoint_auth_method\":\"client_secret_post\""));
        assert!(!json.contains("client_secret_post\":"));
    }

    #[test]
    fn exact_server_and_normalized_url_binding_are_required() {
        let metadata = StoredOAuthMetadata::new(draft(), Default::default()).unwrap();
        assert_eq!(
            metadata.validate_for_binding(
                "server-2",
                "https://mcp.example.test/mcp",
                Default::default(),
            ),
            Err(StoredOAuthMetadataError::ServerMismatch)
        );
        assert_eq!(
            metadata.validate_for_binding(
                "server-1",
                "https://mcp.example.test/other",
                Default::default(),
            ),
            Err(StoredOAuthMetadataError::UrlMismatch)
        );
        // URL parser normalization is deterministic, but a different path/trailing slash remains
        // a different exact binding.
        assert!(
            metadata
                .validate_for_binding(
                    "server-1",
                    " HTTPS://MCP.EXAMPLE.TEST/mcp ",
                    Default::default(),
                )
                .is_ok()
        );
        assert!(
            metadata
                .validate_for_binding(
                    "server-1",
                    "https://mcp.example.test/mcp/",
                    Default::default(),
                )
                .is_err()
        );
    }

    #[test]
    fn unsafe_endpoints_and_unknown_or_basic_auth_methods_fail_closed() {
        let mut unsafe_draft = draft();
        unsafe_draft.token_endpoint = "http://auth.example.test/token".to_owned();
        assert_eq!(
            StoredOAuthMetadata::new(unsafe_draft, Default::default()).unwrap_err(),
            StoredOAuthMetadataError::InvalidEndpoint
        );

        for method in ["client_secret_basic", "vendor_auth"] {
            let json = format!(
                r#"{{"server_id":"server-1","server_url":"https://mcp.example.test/mcp","issuer":"https://auth.example.test/","authorization_endpoint":"https://auth.example.test/authorize","token_endpoint":"https://auth.example.test/token","client_id":"desktop-client","token_endpoint_auth_method":"{method}"}}"#
            );
            assert_eq!(
                StoredOAuthMetadata::from_json_bounded(
                    json.as_bytes(),
                    "server-1",
                    "https://mcp.example.test/mcp",
                    Default::default(),
                )
                .unwrap_err(),
                StoredOAuthMetadataError::UnsupportedAuthMethod
            );
        }
    }

    #[test]
    fn scope_count_and_json_bytes_accept_exact_limit_and_reject_plus_one() {
        let mut exact = draft();
        exact.scopes = vec!["a".to_owned(), "b".to_owned()];
        let scope_limits = StoredOAuthMetadataLimits {
            scopes: 2,
            ..StoredOAuthMetadataLimits::PRODUCTION
        };
        assert!(StoredOAuthMetadata::new(exact, scope_limits).is_ok());

        let mut plus_one = draft();
        plus_one.scopes = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(
            StoredOAuthMetadata::new(plus_one, scope_limits).unwrap_err(),
            StoredOAuthMetadataError::ScopeLimitExceeded
        );

        let metadata = StoredOAuthMetadata::new(draft(), Default::default()).unwrap();
        let json = metadata
            .to_json_bounded(
                "server-1",
                "https://mcp.example.test/mcp",
                Default::default(),
            )
            .unwrap();
        let exact_json_limits = StoredOAuthMetadataLimits {
            json_bytes: json.len(),
            ..StoredOAuthMetadataLimits::PRODUCTION
        };
        assert!(
            metadata
                .to_json_bounded(
                    "server-1",
                    "https://mcp.example.test/mcp",
                    exact_json_limits,
                )
                .is_ok()
        );
        let plus_one_input = format!("{json} ");
        assert_eq!(
            StoredOAuthMetadata::from_json_bounded(
                plus_one_input.as_bytes(),
                "server-1",
                "https://mcp.example.test/mcp",
                exact_json_limits,
            )
            .unwrap_err(),
            StoredOAuthMetadataError::JsonLimitExceeded
        );
    }

    #[test]
    fn debug_is_redacted() {
        let metadata = StoredOAuthMetadata::new(draft(), Default::default()).unwrap();
        let debug = format!("{metadata:?}");
        for forbidden in [
            "mcp.example.test",
            "auth.example.test",
            "desktop-client",
            "server-1",
            "tools.read",
        ] {
            assert!(!debug.contains(forbidden), "{debug}");
        }
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn slack_binding_accepts_endpoint_trailing_slash_and_preserves_origin_resource() {
        let mut slack = draft();
        slack.server_url = "https://mcp.slack.com/mcp/".to_owned();
        slack.oauth_resource = "https://mcp.slack.com/".to_owned();
        slack.provider_workspace_id = Some("T12345678".to_owned());
        slack.workspace_domain = Some("vector9.slack.com".to_owned());
        let metadata = StoredOAuthMetadata::new(slack, Default::default()).unwrap();
        assert_eq!(metadata.oauth_resource(), crate::SLACK_OAUTH_RESOURCE);
        assert_eq!(metadata.provider_workspace_id(), Some("T12345678"));
        assert_eq!(metadata.workspace_domain(), Some("vector9.slack.com"));
    }
}
