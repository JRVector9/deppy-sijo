//! Backend tool discovery projection used by the authorized proxy executor.
//!
//! tools/call is deliberately absent here: the only production call path lives in
//! hook::AuthorizedExecutor and consumes an opaque durable AuthorizationGrant by value.

use serde_json::{Value, json};

pub fn list_tools(discovered: Vec<mcp::McpTool>) -> anyhow::Result<Value> {
    let tools: Vec<Value> = discovered
        .into_iter()
        .map(|tool| {
            let schema: Value =
                serde_json::from_str(&tool.input_schema_json).unwrap_or_else(|_| json!({}));
            let mut object = json!({ "name": tool.name, "inputSchema": schema });
            if let Some(description) = tool.description {
                object["description"] = Value::String(description);
            }
            object
        })
        .collect();
    Ok(json!({ "tools": tools }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_projection은_description과_schema를_보존한다() {
        let result = list_tools(vec![mcp::McpTool {
            name: "read".to_owned(),
            description: Some("Read a file".to_owned()),
            input_schema_json: r#"{"type":"object","required":["path"]}"#.to_owned(),
        }])
        .unwrap();
        assert_eq!(result["tools"][0]["name"], "read");
        assert_eq!(result["tools"][0]["description"], "Read a file");
        assert_eq!(result["tools"][0]["inputSchema"]["type"], "object");
        assert_eq!(result["tools"][0]["inputSchema"]["required"][0], "path");
    }

    #[test]
    fn malformed_schema는_빈_object로_축소하고_raw_error를_노출하지_않는다() {
        let result = list_tools(vec![mcp::McpTool {
            name: "broken".to_owned(),
            description: None,
            input_schema_json: "{raw-secret-parse-error".to_owned(),
        }])
        .unwrap();
        assert_eq!(result["tools"][0]["inputSchema"], json!({}));
        assert!(result["tools"][0].get("description").is_none());
        assert!(!result.to_string().contains("raw-secret"));
    }
}
