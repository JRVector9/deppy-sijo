use serde::{Deserialize, Serialize};

/// The original, shared bearer retains its historical operation namespace.
pub const LEGACY_CONNECTION_ID: &str = "manual-legacy";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    ManualBearer,
    #[serde(rename = "oauth")]
    OAuth,
}

/// Non-secret identity issued by Deppy after credential verification.
/// Client metadata and tool arguments never supply these fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    pub id: String,
    pub name: String,
    pub auth_kind: AuthKind,
    pub input_scope: bool,
    pub authenticated_at: u64,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Active,
    Revoked,
    Expired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub principal: Principal,
    pub last_authenticated_at: Option<u64>,
    pub last_initialize_at: Option<u64>,
    pub last_tool_success_at: Option<u64>,
    pub state: ConnectionState,
}

impl ConnectionInfo {
    pub(crate) fn new(principal: Principal) -> Self {
        Self {
            principal,
            last_authenticated_at: None,
            last_initialize_at: None,
            last_tool_success_at: None,
            state: ConnectionState::Active,
        }
    }
}

pub(crate) fn valid_connection_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}
