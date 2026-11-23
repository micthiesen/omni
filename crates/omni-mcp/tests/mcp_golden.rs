//! The committed MCP snapshots are generated from the Rust tool definitions:
//! `crates/omni-mcp-kit/golden/tools-list.json`, both copies of the policy
//! inventory, and every `tools/list` and `events/list` result embedded in
//! `tests/golden/protocol.json` (the latter from the MCP Events catalog). This test fails when a definition and a snapshot
//! differ; `cargo xtask mcp-golden` (which runs it with `OMNI_MCP_GOLDEN=write`)
//! rewrites the snapshots so the change can be reviewed as a JSON diff.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use crate::common;

use std::path::{Path, PathBuf};

use omni_mcp_kit::ToolMeta;
use omni_mcp_kit::golden::{policy_inventory, pretty, tools_list};
use serde_json::Value;

const TOOLS_LIST: &str = "crates/omni-mcp-kit/golden/tools-list.json";
const POLICY: [&str; 2] = [
    "docs/mcp-policy.json",
    "crates/omni-mcp-kit/golden/mcp-policy.json",
];
const PROTOCOL: &str = "crates/omni-mcp/tests/golden/protocol.json";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|e| panic!("reading {relative}: {e}"))
}

/// `protocol` with every `tools/list` result's `tools` replaced by `tools`
/// and every `events/list` result's `events` by the event catalog.
fn with_tools(mut protocol: Value, tools: &Value) -> (Value, usize) {
    let catalog = omni_mcp::events::catalog::event_catalog();
    let mut replaced = 0;
    for exchange in protocol["exchanges"].as_array_mut().unwrap() {
        let Some(messages) = exchange["response"]["messages"].as_array_mut() else {
            continue;
        };
        for message in messages {
            if let Some(listed) = message
                .get_mut("result")
                .and_then(|result| result.get_mut("tools"))
            {
                *listed = tools.clone();
                replaced += 1;
            }
            if let Some(events) = message
                .get_mut("result")
                .and_then(|result| result.get_mut("events"))
                .filter(|events| events.is_array())
            {
                *events = catalog.clone();
            }
        }
    }
    (protocol, replaced)
}

/// Every snapshot as the definitions render it.
fn rendered() -> Vec<(&'static str, String)> {
    let metas: Vec<&ToolMeta> = common::all_defs()
        .into_iter()
        .map(|def| def.meta().unwrap_or_else(|e| panic!("{e}")))
        .collect();
    let list = tools_list(&metas);
    let policy = pretty(&policy_inventory(&metas));
    let current: Value = serde_json::from_str(&read(PROTOCOL)).unwrap();
    let (protocol, replaced) = with_tools(current, &list["tools"]);
    assert!(replaced > 0, "{PROTOCOL} has no tools/list results");
    let mut out = vec![(TOOLS_LIST, pretty(&list))];
    out.extend(POLICY.map(|path| (path, policy.clone())));
    out.push((PROTOCOL, pretty(&protocol)));
    out
}

#[test]
fn committed_snapshots_match_the_tool_definitions() {
    let write = std::env::var("OMNI_MCP_GOLDEN").is_ok_and(|v| v == "write");
    let mut stale = Vec::new();
    for (relative, text) in rendered() {
        if read(relative) == text {
            continue;
        }
        if write {
            std::fs::write(repo_root().join(relative), &text).unwrap();
            println!("mcp-golden: wrote {relative}");
        } else {
            stale.push(relative);
        }
    }
    assert!(
        stale.is_empty(),
        "{stale:?} differ from the MCP tool definitions; run `cargo xtask mcp-golden` and review the diff"
    );
}

#[test]
fn every_definition_is_served_once_in_order() {
    let names: Vec<&str> = common::all_defs().iter().map(|def| def.name()).collect();
    assert_eq!(names, omni_mcp::tools::TOOL_ORDER);
}
