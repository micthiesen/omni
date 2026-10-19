//! The MCP policy inventory. The registered tool set is this package's tools
//! plus every other package's definitions, so the inventory is the committed
//! one. The machine-word lint runs over the derived JSON schemas the endpoint
//! serves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::{FakeTasks, clock, other_tools};
use omni_mcp::policy::serialize_policy_inventory;
use omni_mcp::tools::claude_sessions::{ClaudeDeps, claude_session_tools};
use omni_mcp::tools::events::event_tools;
use omni_mcp::tools::system::{ConfiguredFeatures, SystemDeps, system_tools};
use omni_mcp_kit::{ApprovalPolicy, McpTool};
use omni_runtime::ports::Ports;
use serde_json::Value;

fn policy_tools() -> Vec<McpTool> {
    let mut own = system_tools(&SystemDeps {
        tasks: Arc::new(FakeTasks::default()),
        ports: Ports::default(),
        features: ConfiguredFeatures::default(),
        clock: clock(),
    })
    .unwrap();
    own.extend(event_tools(None).unwrap());
    own.extend(
        claude_session_tools(&ClaudeDeps {
            host: None,
            watcher: None,
        })
        .unwrap(),
    );
    let mut tools = other_tools(&own);
    tools.extend(own);
    tools
}

fn repo_file(path: &str) -> String {
    std::fs::read_to_string(format!("{}/../../{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn is_complete_and_generated_from_the_registered_tool_definitions() {
    let tools = policy_tools();
    assert_eq!(
        repo_file("docs/mcp-policy.json"),
        serialize_policy_inventory(&tools)
    );
    let names: HashSet<&str> = tools.iter().map(|t| t.meta.name.as_str()).collect();
    assert_eq!(names.len(), tools.len());
    assert!(tools.len() > 50);
}

#[test]
fn records_every_annotation_and_a_usable_approval_policy_for_every_tool() {
    for tool in policy_tools() {
        let meta = tool.meta;
        let annotations = serde_json::to_value(meta.annotations).unwrap();
        assert_eq!(annotations.as_object().unwrap().len(), 4, "{}", meta.name);
        assert!(matches!(
            meta.policy.recommended_policy,
            ApprovalPolicy::Allow | ApprovalPolicy::RequireApproval | ApprovalPolicy::Block
        ));
        assert!(!meta.policy.cost.is_empty(), "{}", meta.name);
        assert!(meta.description.chars().count() > 20, "{}", meta.name);
    }
}

fn mentions_machine(text: &str) -> bool {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    [
        "mac", "macs", "macbook", "maxbook", "macos", "laptop", "eventkit",
    ]
    .iter()
    .any(|word| words.contains(word))
}

#[test]
fn never_names_the_machine_behind_any_tool() {
    let mut offenders = Vec::new();
    for tool in policy_tools() {
        let meta = tool.meta;
        let text = serde_json::to_string(&serde_json::json!([
            meta.title,
            meta.description,
            meta.policy,
            Value::Object((*meta.input_schema).clone()),
            Value::Object((*meta.output_schema).clone()),
        ]))
        .unwrap();
        if mentions_machine(&text) {
            offenders.push(meta.name.clone());
        }
    }
    let instructions =
        omni_mcp_kit::golden::handshake().unwrap()["legacy"]["initialize"]["instructions"]
            .as_str()
            .unwrap()
            .to_owned();
    if mentions_machine(&instructions) {
        offenders.push("server instructions".to_owned());
    }
    let catalog = omni_mcp::events::catalog::event_catalog().to_string();
    if mentions_machine(&catalog) {
        offenders.push("event catalog".to_owned());
    }
    assert!(offenders.is_empty(), "{offenders:?}");
    assert!(mentions_machine("Runs on the Mac"));
    assert!(!mentions_machine("machine"));
}
