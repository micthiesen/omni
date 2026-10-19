//! The approval policy inventory: every registered tool
//! sorted by `localeCompare` on its name, serialized as `docs/mcp-policy.json`
//! (`JSON.stringify(inventory, null, 2)` plus a trailing newline).

use omni_mcp_kit::{McpTool, ToolMeta};
use serde_json::Value;

pub use omni_mcp_kit::golden::POLICY_SCHEMA_VERSION as MCP_POLICY_SCHEMA_VERSION;

pub fn build_policy_inventory(tools: &[McpTool]) -> Value {
    let metas: Vec<&ToolMeta> = tools.iter().map(|tool| tool.meta).collect();
    omni_mcp_kit::golden::policy_inventory(&metas)
}

pub fn serialize_policy_inventory(tools: &[McpTool]) -> String {
    omni_mcp_kit::golden::pretty(&build_policy_inventory(tools))
}
