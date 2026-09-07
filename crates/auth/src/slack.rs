//! Slack-specific OAuth helpers with bounded inputs and sanitized errors.

use std::io::Read as _;
use std::time::Duration;

pub const SLACK_MCP_URL: &str = "https://mcp.slack.com/mcp";
pub const SLACK_OAUTH_RESOURCE: &str = "https://mcp.slack.com";
pub const SLACK_WORKSPACE_HTML_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct SlackWorkspaceTarget {
    domain: String,
    team_id: String,
}

impl SlackWorkspaceTarget {
    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }
}

impl std::fmt::Debug for SlackWorkspaceTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlackWorkspaceTarget")
            .field("binding", &"REDACTED")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackWorkspaceError {
    Cancelled,
    InvalidDomain,
    InvalidResponse,
    ResponseLimitExceeded,
    RequestFailed,
}

impl std::fmt::Display for SlackWorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Cancelled => "Slack workspace resolution was cancelled",
            Self::InvalidDomain => "Slack workspace domain is invalid",
            Self::InvalidResponse => "Slack workspace response is invalid",
            Self::ResponseLimitExceeded => "Slack workspace response exceeds its byte limit",
            Self::RequestFailed => "Slack workspace request failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SlackWorkspaceError {}

/// Accepts a Slack slug, host, or HTTPS URL and returns the canonical lower-case host. Paths,
/// credentials, queries, fragments, nested subdomains, and plain HTTP are rejected.
pub fn normalize_slack_workspace_domain(input: &str) -> Result<String, SlackWorkspaceError> {
    let input = input.trim();
    if input.is_empty() || input.len() > 100 {
        return Err(SlackWorkspaceError::InvalidDomain);
    }
    let input = input.to_ascii_lowercase();
    if input.starts_with("http://") {
        return Err(SlackWorkspaceError::InvalidDomain);
    }
    let without_scheme = input.strip_prefix("https://").unwrap_or(&input);
    let host = without_scheme.trim_end_matches('/');
    if host.contains('/') || host.contains('?') || host.contains('#') || host.contains('@') {
        return Err(SlackWorkspaceError::InvalidDomain);
    }
    let slug = host.strip_suffix(".slack.com").unwrap_or(host);
    let valid_edges = slug
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && slug
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if slug.is_empty()
        || slug.len() > 80
        || slug.contains('.')
        || !valid_edges
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(SlackWorkspaceError::InvalidDomain);
    }
    Ok(format!("{slug}.slack.com"))
}

pub fn is_valid_slack_team_id(team_id: &str) -> bool {
    (9..=32).contains(&team_id.len())
        && team_id.starts_with('T')
        && team_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

pub fn parse_slack_workspace_html(
    domain: &str,
    html: &[u8],
) -> Result<SlackWorkspaceTarget, SlackWorkspaceError> {
    if html.len() > SLACK_WORKSPACE_HTML_MAX_BYTES {
        return Err(SlackWorkspaceError::ResponseLimitExceeded);
    }
    let domain = normalize_slack_workspace_domain(domain)?;
    let html = std::str::from_utf8(html).map_err(|_| SlackWorkspaceError::InvalidResponse)?;
    let expected_slug = domain.trim_end_matches(".slack.com");
    let returned_slug = slack_html_string_property(html, "teamDomain")
        .ok_or(SlackWorkspaceError::InvalidResponse)?;
    if returned_slug != expected_slug {
        return Err(SlackWorkspaceError::InvalidResponse);
    }
    let team_id = slack_html_string_property(html, "encodedTeamId")
        .ok_or(SlackWorkspaceError::InvalidResponse)?;
    if !is_valid_slack_team_id(&team_id) {
        return Err(SlackWorkspaceError::InvalidResponse);
    }
    Ok(SlackWorkspaceTarget { domain, team_id })
}

/// Resolves a Slack workspace without redirects. Cancellation is checked before network I/O,
/// before body allocation, and after the bounded read. A blocking request remains bounded by the
/// supplied timeout; no background thread is detached.
pub fn resolve_slack_workspace_cancellable(
    timeout: Duration,
    input: &str,
    cancelled: impl Fn() -> bool,
) -> Result<SlackWorkspaceTarget, SlackWorkspaceError> {
    let domain = normalize_slack_workspace_domain(input)?;
    if cancelled() {
        return Err(SlackWorkspaceError::Cancelled);
    }
    let url = format!("https://{domain}/");
    let http = ureq::Agent::config_builder()
        .timeout_connect(Some(timeout))
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .build()
        .new_agent();
    let response = http
        .get(&url)
        .header("Accept", "text/html")
        .header("User-Agent", "Deppy-Sijo/Slack-Workspace-Resolver")
        .call()
        .map_err(|_| SlackWorkspaceError::RequestFailed)?;
    if response.status().as_u16() != 200 {
        return Err(SlackWorkspaceError::InvalidResponse);
    }
    if cancelled() {
        return Err(SlackWorkspaceError::Cancelled);
    }
    let probe = SLACK_WORKSPACE_HTML_MAX_BYTES
        .checked_add(1)
        .ok_or(SlackWorkspaceError::ResponseLimitExceeded)?;
    let mut html = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(probe as u64)
        .read_to_end(&mut html)
        .map_err(|_| SlackWorkspaceError::RequestFailed)?;
    if html.len() > SLACK_WORKSPACE_HTML_MAX_BYTES {
        return Err(SlackWorkspaceError::ResponseLimitExceeded);
    }
    if cancelled() {
        return Err(SlackWorkspaceError::Cancelled);
    }
    parse_slack_workspace_html(&domain, &html)
}

/// Extracts only a canonical Slack app settings URL from the known MCP-access error shape.
pub fn slack_mcp_enable_url(message: &str) -> Option<String> {
    if message.len() > 16 * 1024
        || !message.contains("App is not enabled for Slack MCP server access")
    {
        return None;
    }
    let normalized = message.replace("\\/", "/");
    let rest = normalized.split_once("https://api.slack.com/apps/")?.1;
    let (app_id, tail) = rest.split_once('/')?;
    if tail != "app-assistant" && !tail.starts_with("app-assistant\"") {
        return None;
    }
    if !(9..=32).contains(&app_id.len())
        || !app_id.starts_with('A')
        || !app_id.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(format!("https://api.slack.com/apps/{app_id}/app-assistant"))
}

pub fn slack_callback_redirect_uri() -> String {
    format!("http://localhost:{}/callback", crate::FIXED_CALLBACK_PORT)
}

fn slack_html_string_property(html: &str, key: &str) -> Option<String> {
    let marker = format!("&quot;{key}&quot;:&quot;");
    let value = html.split_once(&marker)?.1.split_once("&quot;")?.0;
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_domain_and_team_validation_are_strict() {
        assert_eq!(
            normalize_slack_workspace_domain(" HTTPS://Vector-9.Slack.com/ ").unwrap(),
            "vector-9.slack.com"
        );
        for invalid in [
            "http://vector9.slack.com",
            "a.b.slack.com",
            "-bad.slack.com",
            "bad-.slack.com",
            "vector9.slack.com/path",
            "user@vector9.slack.com",
        ] {
            assert!(
                normalize_slack_workspace_domain(invalid).is_err(),
                "{invalid}"
            );
        }
        assert!(is_valid_slack_team_id("T12345678"));
        assert!(!is_valid_slack_team_id("A12345678"));
        assert!(!is_valid_slack_team_id("T-short"));
    }

    #[test]
    fn oversized_workspace_input_is_rejected_before_normalization_allocation() {
        let oversized = "A".repeat(101);
        assert_eq!(
            normalize_slack_workspace_domain(&oversized).unwrap_err(),
            SlackWorkspaceError::InvalidDomain
        );
    }

    #[test]
    fn workspace_html_is_bound_and_domain_bound() {
        let html = br#"window.boot = {&quot;teamDomain&quot;:&quot;vector9&quot;,&quot;encodedTeamId&quot;:&quot;T12345678&quot;};"#;
        let target = parse_slack_workspace_html("vector9.slack.com", html).unwrap();
        assert_eq!(target.domain(), "vector9.slack.com");
        assert_eq!(target.team_id(), "T12345678");
        assert!(parse_slack_workspace_html("other.slack.com", html).is_err());
        assert_eq!(
            parse_slack_workspace_html(
                "vector9.slack.com",
                &vec![b'x'; SLACK_WORKSPACE_HTML_MAX_BYTES + 1],
            )
            .unwrap_err(),
            SlackWorkspaceError::ResponseLimitExceeded
        );
    }

    #[test]
    fn workspace_network_primitive_honors_preflight_cancellation() {
        let result = resolve_slack_workspace_cancellable(
            Duration::from_secs(1),
            "vector9.slack.com",
            || true,
        );
        assert_eq!(result.unwrap_err(), SlackWorkspaceError::Cancelled);
    }

    #[test]
    fn settings_link_is_canonical_or_absent() {
        let message = "App is not enabled for Slack MCP server access: https://api.slack.com/apps/A12345678/app-assistant";
        assert_eq!(
            slack_mcp_enable_url(message).as_deref(),
            Some("https://api.slack.com/apps/A12345678/app-assistant")
        );
        assert!(slack_mcp_enable_url("https://evil.test/apps/A12345678").is_none());
    }

    #[test]
    fn debug_hides_workspace_identifiers() {
        let target = SlackWorkspaceTarget {
            domain: "vector9.slack.com".to_owned(),
            team_id: "T12345678".to_owned(),
        };
        let debug = format!("{target:?}");
        assert!(!debug.contains("vector9"));
        assert!(!debug.contains("T12345678"));
    }
}
