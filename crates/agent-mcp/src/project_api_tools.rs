//! Scoped project API tools for ordinary shared sessions, separate from MAIN authority.
use serde_json::{Value, json};

pub fn is_project_api_tool(name: &str) -> bool {
    matches!(name, "list_project_apis" | "run_project_api_recipe")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectApiCall {
    List {
        session_id: String,
        generation: String,
    },
    Recipe {
        session_id: String,
        generation: String,
        operation_id: String,
        recipe_id: String,
        key_ids: Vec<String>,
    },
}

pub fn project_api_tools() -> Vec<Value> {
    let identity = json!({"session_id":{"type":"string","minLength":1,"maxLength":128},"generation":{"type":"string","minLength":1,"maxLength":128}});
    let mut recipe = identity.as_object().unwrap().clone();
    recipe.extend(json!({"operation_id":{"type":"string","minLength":1,"maxLength":128},"recipe_id":{"type":"string","minLength":1,"maxLength":128},"key_ids":{"type":"array","minItems":1,"maxItems":256,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":128}}}).as_object().unwrap().clone());
    vec![
        json!({"name":"list_project_apis","description":"Discover value-free project API metadata for this exact shared session. Session read/input sharing does not grant secret use; only separately granted APIs are returned. Never request or print secret values.","inputSchema":{"type":"object","properties":identity,"required":["session_id","generation"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"run_project_api_recipe","description":"Execute one saved recipe with an explicit API key ID subset only when this authenticated connection has separate recipe grants for the exact session. Returns a value-free execution receipt. Reuse the operation ID only for this exact action; query get_operation after an uncertain result.","inputSchema":{"type":"object","properties":recipe,"required":["session_id","generation","operation_id","recipe_id","key_ids"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"idempotentHint":true}}),
    ]
}

pub fn parse_project_api_call(name: &str, args: &Value) -> Result<ProjectApiCall, &'static str> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ListArgs {
        session_id: String,
        generation: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RecipeArgs {
        session_id: String,
        generation: String,
        operation_id: String,
        recipe_id: String,
        key_ids: Vec<String>,
    }
    let valid = |id: &str| !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control);
    match name {
        "list_project_apis" => {
            let args: ListArgs =
                serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
            if !valid(&args.session_id) || !valid(&args.generation) {
                return Err("invalid_arguments");
            }
            Ok(ProjectApiCall::List {
                session_id: args.session_id,
                generation: args.generation,
            })
        }
        "run_project_api_recipe" => {
            let args: RecipeArgs =
                serde_json::from_value(args.clone()).map_err(|_| "invalid_arguments")?;
            if [
                &args.session_id,
                &args.generation,
                &args.operation_id,
                &args.recipe_id,
            ]
            .iter()
            .any(|id| !valid(id))
                || args.key_ids.is_empty()
                || args.key_ids.len() > 256
                || args
                    .key_ids
                    .iter()
                    .enumerate()
                    .any(|(index, id)| !valid(id) || args.key_ids[..index].contains(id))
            {
                return Err("invalid_arguments");
            }
            Ok(ProjectApiCall::Recipe {
                session_id: args.session_id,
                generation: args.generation,
                operation_id: args.operation_id,
                recipe_id: args.recipe_id,
                key_ids: args.key_ids,
            })
        }
        _ => Err("unknown_tool"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_api_tools_scope_by_session_without_plain_secret_or_global_workspace_arguments() {
        let tools = project_api_tools();
        assert_eq!(tools.len(), 2);
        for tool in tools {
            assert!(tool["inputSchema"]["properties"]["workspace_id"].is_null());
            assert!(tool["inputSchema"]["properties"]["value"].is_null());
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        let list = parse_project_api_call(
            "list_project_apis",
            &json!({"session_id":"session","generation":"runtime-generation"}),
        )
        .unwrap();
        assert!(matches!(list, ProjectApiCall::List { .. }));
        assert!(parse_project_api_call("list_project_apis",&json!({"session_id":"session","generation":"runtime-generation","workspace_id":"other"})).is_err());
        assert!(!crate::is_main_tool("list_project_apis"));
    }
    #[test]
    fn project_api_recipe_requires_explicit_unique_keys_and_one_operation() {
        let args = json!({"session_id":"session","generation":"generation","operation_id":"once","recipe_id":"recipe","key_ids":["key"]});
        assert!(matches!(
            parse_project_api_call("run_project_api_recipe", &args).unwrap(),
            ProjectApiCall::Recipe { .. }
        ));
        let mut duplicate = args.clone();
        duplicate["key_ids"] = json!(["key", "key"]);
        assert!(parse_project_api_call("run_project_api_recipe", &duplicate).is_err());
        let mut empty = args.clone();
        empty["key_ids"] = json!([]);
        assert!(parse_project_api_call("run_project_api_recipe", &empty).is_err());
        let mut command = args;
        command["command"] = json!(["sh", "-c", "arbitrary"]);
        assert!(parse_project_api_call("run_project_api_recipe", &command).is_err());
    }
    #[test]
    fn project_api_tools_require_explicit_host_availability_outside_main() {
        let normal = crate::tools();
        for name in ["list_project_apis", "run_project_api_recipe"] {
            assert!(
                !normal["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == name)
            );
            assert!(
                crate::tools_with_project_api_access(true)["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == name)
            );
            assert!(
                !crate::main_tools()["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["name"] == name)
            );
        }
    }
}
