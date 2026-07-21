//! MCP protocol/resource ceilings shared by stdio and HTTP transports.
//!
//! These values mirror `connector_contract::ResourceLimits::PRODUCTION_CEILING`
//! without adding a dependency from this protocol crate to the UI contract crate.
//! Raising one requires changing both locations with measurement evidence.

use anyhow::bail;
use serde_json::Value;

/// Maximum tools retained for one server discovery.
pub const MAX_TOOLS_PER_SERVER: usize = 4_096;
/// Maximum cumulative serialized bytes of tool descriptors for one discovery.
pub const MAX_TOOL_DESCRIPTOR_BYTES: usize = 8 * 1024 * 1024;
/// Maximum serialized `tools/call` arguments.
pub const MAX_TOOL_INPUT_BYTES: usize = 32 * 1024;
/// Maximum raw JSON-RPC response message/body.
pub const MAX_RAW_MCP_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum concurrent blocking HTTP sends, including sends whose caller timed out.
pub const MAX_HTTP_SENDS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpPayloadKind {
    ToolInput,
    ToolDescriptors,
    RawResponse,
}

impl McpPayloadKind {
    pub const fn max_bytes(self) -> usize {
        match self {
            Self::ToolInput => MAX_TOOL_INPUT_BYTES,
            Self::ToolDescriptors => MAX_TOOL_DESCRIPTOR_BYTES,
            Self::RawResponse => MAX_RAW_MCP_RESPONSE_BYTES,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::ToolInput => "MCP tool input",
            Self::ToolDescriptors => "MCP tool descriptors",
            Self::RawResponse => "MCP raw response",
        }
    }
}

pub fn enforce_payload_bytes(kind: McpPayloadKind, actual: usize) -> anyhow::Result<()> {
    enforce_bytes(kind.label(), actual, kind.max_bytes())
}

pub fn enforce_json_payload(kind: McpPayloadKind, value: &Value) -> anyhow::Result<usize> {
    enforce_json_bytes(kind.label(), value, kind.max_bytes())
}

pub(crate) fn serialized_json_len(value: &Value) -> anyhow::Result<usize> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(anyhow::Error::from)
}

pub(crate) fn enforce_json_bytes(
    label: &str,
    value: &Value,
    max_bytes: usize,
) -> anyhow::Result<usize> {
    let actual = serialized_json_len(value)?;
    enforce_bytes(label, actual, max_bytes)?;
    Ok(actual)
}

pub(crate) fn enforce_bytes(label: &str, actual: usize, max_bytes: usize) -> anyhow::Result<()> {
    if actual > max_bytes {
        bail!("{label} 크기 초과: {actual} bytes (max {max_bytes})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_byte_limit_is_inclusive_and_counts_utf8_bytes() {
        let value = json!({"text": "가"});
        let exact = serialized_json_len(&value).unwrap();
        assert_eq!(enforce_json_bytes("fixture", &value, exact).unwrap(), exact);
        let error = enforce_json_bytes("fixture", &value, exact - 1).unwrap_err();
        assert!(format!("{error:#}").contains("크기 초과"));
    }

    #[test]
    fn public_payload_ceilings_match_frozen_connector_contract() {
        assert_eq!(MAX_TOOLS_PER_SERVER, 4_096);
        assert_eq!(McpPayloadKind::ToolDescriptors.max_bytes(), 8 * 1024 * 1024);
        assert_eq!(McpPayloadKind::ToolInput.max_bytes(), 32 * 1024);
        assert_eq!(McpPayloadKind::RawResponse.max_bytes(), 8 * 1024 * 1024);
        assert_eq!(MAX_HTTP_SENDS, 2);
    }
}
