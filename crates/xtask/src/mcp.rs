//! The MCP snapshots generated from the Rust tool definitions.
//!
//! Each tool's contract is a `ToolDef` in its package's `defs` module; the
//! committed `crates/omni-mcp-kit/golden/tools-list.json`, both copies of the
//! policy inventory and the `tools/list` results inside
//! `crates/omni-mcp/tests/golden/protocol.json` are rendered from them by the
//! `omni-mcp` test `mcp_golden`, the only crate that sees every package's tools.
//! `mcp-golden` runs that test in write mode; `--check` runs it as a check.

use anyhow::{Context, Result, bail};

use crate::cargo_command;

/// Regenerates the snapshots, or with `check` fails when any differs.
pub fn mcp_golden(check: bool) -> Result<()> {
    let mut command = cargo_command();
    command
        .args(["test", "--locked", "-p", "omni-mcp", "--test", "mcp_golden"])
        .args(["--", "--nocapture"]);
    if check {
        command.env_remove("OMNI_MCP_GOLDEN");
    } else {
        command.env("OMNI_MCP_GOLDEN", "write");
    }
    let status = command.status().context("starting cargo test")?;
    if !status.success() {
        if check {
            bail!("MCP snapshots differ from the tool definitions; run `cargo xtask mcp-golden`");
        }
        bail!("generating the MCP snapshots failed ({status})");
    }
    println!(
        "mcp-golden: snapshots {}",
        if check { "match" } else { "are up to date" }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::repo_root;

    /// The published policy inventory and the golden copy.
    const POLICY_PATHS: [&str; 2] = [
        "docs/mcp-policy.json",
        "crates/omni-mcp-kit/golden/mcp-policy.json",
    ];

    fn read(relative: &str) -> Value {
        let text = std::fs::read_to_string(repo_root().join(relative)).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The registry check lives in `omni-mcp`; this keeps the committed copies
    /// consistent with each other without building every subsystem.
    #[test]
    fn snapshots_agree_with_each_other() {
        let [docs, golden] =
            POLICY_PATHS.map(|p| std::fs::read_to_string(repo_root().join(p)).unwrap());
        assert_eq!(docs, golden);
        let policy = read(POLICY_PATHS[0]);
        let list = read("crates/omni-mcp-kit/golden/tools-list.json");
        let names = |value: &Value, key: &str| -> Vec<String> {
            let mut names: Vec<String> = value[key]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap().to_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&policy, "tools"), names(&list, "tools"));
    }
}
