//! Tool definitions, schema validation and result formatting.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use futures::future::BoxFuture;
use omni_mcp_kit::registry::{
    RegistryError, RegistryServer, ToolRegistry, call_tool_result, standalone_context,
};
use omni_mcp_kit::{
    Annotations, ApprovalPolicy, Policy, ToolContext, ToolDef, ToolDefinition, ToolError,
    ToolHandler, ToolInfo, ToolOutput, ToolPhase, raw_tool, typed_tool,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const READ_ONLY: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

const ALLOW: Policy = Policy {
    side_effects: &[],
    cost: "none",
    recommended: ApprovalPolicy::Allow,
};

// Schema-only types are never constructed.
#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct ListSchema {
    #[schemars(length(min = 1, max = 200))]
    briefing_name: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct ListOutputSchema {
    briefing_names: Vec<String>,
    next_cursor: Option<u64>,
    total: u64,
}

static LIST: ToolDef<ListSchema, ListOutputSchema> = ToolDef::new(ToolInfo {
    name: "briefings_list",
    title: "List Briefings",
    description: "List briefings.",
    annotations: READ_ONLY,
    policy: ALLOW,
});

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Empty {}

static BROWSE: ToolDef<Empty, Empty> = ToolDef::new(ToolInfo {
    name: "browse_browser_history",
    title: "Browse",
    description: "Browse.",
    annotations: READ_ONLY,
    policy: Policy {
        side_effects: &["Reads history"],
        cost: "none",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListInput {
    briefing_name: Option<String>,
    #[serde(default)]
    cursor: u32,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    25
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListOutput {
    briefing_names: Vec<String>,
    next_cursor: Option<u32>,
    total: u32,
}

fn list_tool() -> omni_mcp_kit::McpTool {
    typed_tool(&LIST, |input: ListInput, _cx| async move {
        Ok::<_, ToolError>(ListOutput {
            briefing_names: input.briefing_name.into_iter().collect(),
            next_cursor: None,
            total: input.cursor + input.limit,
        })
    })
    .unwrap()
}

fn cx() -> ToolContext {
    standalone_context("test")
}

#[test]
fn definitions_build_metadata_with_derived_schemas() {
    let meta = LIST.meta().unwrap();
    assert_eq!(meta.name, "briefings_list");
    assert_eq!(meta.annotations, READ_ONLY);
    assert_eq!(meta.policy.recommended_policy, ApprovalPolicy::Allow);
    assert_eq!(
        omni_core::js::json_stringify(&Value::Object((*meta.input_schema).clone())),
        concat!(
            r#"{"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema","#,
            r#""properties":{"briefingName":{"type":"string","minLength":1,"maxLength":200},"#,
            r#""cursor":{"default":0,"description":"Zero-based result offset","type":"integer","#,
            r#""minimum":0,"maximum":9007199254740991},"limit":{"default":25,"type":"integer","#,
            r#""minimum":1,"maximum":100}},"additionalProperties":false}"#
        )
    );
    assert_eq!(
        omni_core::js::json_stringify(&Value::Object((*meta.output_schema).clone())),
        concat!(
            r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","#,
            r#""properties":{"briefingNames":{"type":"array","items":{"type":"string"}},"#,
            r#""nextCursor":{"anyOf":[{"type":"integer","minimum":0,"maximum":9007199254740991},"#,
            r#"{"type":"null"}]},"total":{"type":"integer","minimum":0,"#,
            r#""maximum":9007199254740991}},"required":["briefingNames","nextCursor","total"],"#,
            r#""additionalProperties":false}"#
        )
    );
    let listed = meta.listed();
    let keys: Vec<&str> = listed.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "name",
            "title",
            "description",
            "inputSchema",
            "annotations",
            "outputSchema"
        ]
    );
}

#[tokio::test]
async fn typed_tools_validate_input_against_the_derived_schema() {
    let tool = list_tool();
    let ok = tool
        .handler
        .call(json!({"briefingName": "Daily"}), cx())
        .await;
    match ok {
        Ok(ToolOutput::Structured(map)) => {
            assert_eq!(map["total"], json!(25));
            assert_eq!(map["briefingNames"], json!(["Daily"]));
        }
        other => panic!("unexpected {other:?}"),
    }
    let too_big = tool.handler.call(json!({"limit": 101}), cx()).await;
    assert!(matches!(
        too_big,
        Err(ToolError {
            phase: ToolPhase::Input,
            ..
        })
    ));
    let extra = tool.handler.call(json!({"unknown": true}), cx()).await;
    assert!(matches!(
        extra,
        Err(ToolError {
            phase: ToolPhase::Input,
            ..
        })
    ));
}

#[tokio::test]
async fn typed_tools_validate_output_against_the_derived_schema() {
    let tool = typed_tool(&LIST, |_: ListInput, _cx| async move {
        Ok::<_, ToolError>(json!({"briefingNames": "not-an-array"}))
    })
    .unwrap();
    let result = tool.handler.call(json!({}), cx()).await;
    assert!(matches!(
        result,
        Err(ToolError {
            phase: ToolPhase::Output,
            ..
        })
    ));
}

struct Failing;

impl ToolHandler for Failing {
    fn call<'a>(
        &'a self,
        _input: Value,
        _cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async { Err(ToolError::execute("innermost cause")) })
    }
}

#[tokio::test]
async fn registry_lists_in_serving_order_and_rejects_mismatches() {
    let failing = raw_tool(&BROWSE, Arc::new(Failing)).unwrap();
    let order = ["briefings_list", "browse_browser_history"];
    let registry = ToolRegistry::new(vec![failing.clone(), list_tool()], &order).unwrap();
    let names: Vec<&str> = registry.listed().iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, order);
    assert!(matches!(
        ToolRegistry::new(vec![failing.clone()], &order),
        Err(RegistryError::OrderMismatch { missing, unexpected })
            if missing == ["briefings_list"] && unexpected.is_empty()
    ));
    assert!(matches!(
        ToolRegistry::new(vec![failing.clone(), list_tool()], &["briefings_list"]),
        Err(RegistryError::OrderMismatch { missing, unexpected })
            if missing.is_empty() && unexpected == ["browse_browser_history"]
    ));
    assert!(matches!(
        ToolRegistry::new(vec![failing.clone(), failing], &order),
        Err(RegistryError::Duplicate(name)) if name == "browse_browser_history"
    ));
    let failed = registry
        .call("browse_browser_history", None, cx())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(call_tool_result(failed)).unwrap(),
        json!({"content": [{"type": "text", "text": "innermost cause"}], "isError": true})
    );
    assert!(registry.call("nope", None, cx()).await.is_none());
}

/// rmcp's `Tool` orders its own fields; the endpoint serves `ToolMeta::listed`.
#[test]
fn listed_tools_carry_the_metadata() {
    let registry = ToolRegistry::new(vec![list_tool()], &["briefings_list"]).unwrap();
    let listed = serde_json::to_value(&registry.listed()[0]).unwrap();
    assert_eq!(listed, Value::Object(LIST.meta().unwrap().listed()));
}

#[test]
fn success_results_mirror_successful_tool_result() {
    let mut structured = Map::new();
    structured.insert("b".to_owned(), json!(1));
    structured.insert("a".to_owned(), json!("x"));
    let result = call_tool_result(Ok(ToolOutput::Structured(structured)));
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({
            "content": [{"type": "text", "text": "{\"b\":1,\"a\":\"x\"}"}],
            "structuredContent": {"b": 1, "a": "x"}
        })
    );
}

#[test]
fn registry_server_uses_golden_server_info() {
    let server = RegistryServer::new(ToolRegistry::new(Vec::new(), &[]).unwrap()).unwrap();
    let info = rmcp::handler::server::ServerHandler::get_info(&server);
    assert_eq!(info.server_info.name, "omni");
    assert!(
        info.instructions
            .unwrap()
            .starts_with("Omni exposes private personal data")
    );
}
