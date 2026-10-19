//! Committed MCP snapshots, embedded at build time, and their renderers.
//!
//! - `golden/tools-list.json`: the served `tools/list` result (identical in the
//!   2025-11-25 and 2026-07-28 eras). Generated from the tool definitions by
//!   `cargo xtask mcp-golden`; never edit it by hand.
//! - `golden/mcp-policy.json`: a copy of `docs/mcp-policy.json`, generated the
//!   same way.
//! - `golden/handshake.json`: the legacy `initialize` result, the modern
//!   `server/discover` result and each era's `tools/list` envelope.

use serde_json::{Value, json};

use crate::{ToolMeta, ToolMetaError};

pub const TOOLS_LIST_JSON: &str = include_str!("../golden/tools-list.json");
pub const HANDSHAKE_JSON: &str = include_str!("../golden/handshake.json");
pub const POLICY_JSON: &str = include_str!("../golden/mcp-policy.json");

/// `schemaVersion` of the policy inventory.
pub const POLICY_SCHEMA_VERSION: u32 = 2;

/// The golden `handshake.json`.
pub fn handshake() -> Result<Value, ToolMetaError> {
    serde_json::from_str(HANDSHAKE_JSON).map_err(|e| ToolMetaError::Handshake(e.to_string()))
}

/// The `tools/list` result for `metas` in serving order.
pub fn tools_list(metas: &[&ToolMeta]) -> Value {
    let tools: Vec<Value> = metas
        .iter()
        .map(|meta| Value::Object(meta.listed()))
        .collect();
    json!({ "tools": tools })
}

/// The policy inventory: tools sorted by `localeCompare` on name.
pub fn policy_inventory(metas: &[&ToolMeta]) -> Value {
    let mut sorted: Vec<&ToolMeta> = metas.to_vec();
    sorted.sort_by(|a, b| omni_core::js::locale_compare(&a.name, &b.name));
    let tools: Vec<Value> = sorted
        .into_iter()
        .map(|meta| {
            json!({
                "name": meta.name,
                "title": meta.title,
                "description": meta.description,
                "annotations": meta.annotations,
                "sideEffects": meta.policy.side_effects,
                "cost": meta.policy.cost,
                "recommendedPolicy": meta.policy.recommended_policy,
            })
        })
        .collect();
    json!({
        "schemaVersion": POLICY_SCHEMA_VERSION,
        "generatedFrom": "omni-mcp tool definitions",
        "tools": tools,
    })
}

/// Pretty JSON with two-space indent and a trailing newline, as committed.
pub fn pretty(value: &Value) -> String {
    format!("{}\n", omni_core::js::json_stringify_pretty2(value))
}
