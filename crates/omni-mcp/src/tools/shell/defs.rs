//! The shell tools' contracts: metadata, policy and the schema types their
//! input and output schemas derive from (`omni_mcp_kit::schema`). These types
//! describe the wire format only; the handlers decode and encode with their
//! own types. After changing anything here run `cargo xtask mcp-golden` and
//! review the snapshot diff.

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

const READ_ONLY: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

pub static SHELL: ToolDef<ShellInput, ShellOutput> = ToolDef::new(ToolInfo {
    name: "shell",
    title: "Run an Omni Shell Script",
    description: "Run a bash script in a sandboxed virtual shell over Omni's data, then return only what the script prints. Every read-only Omni tool is a command that prints its JSON result (`email_search --from amazon --limit 5`, `parcels_get --tracking-number 1Z...`, or one JSON object argument); `tools` lists them and `<tool> --help` shows its flags. Reads that need no arguments are also files: `/omni/<tool>.json`. Filter with jq, grep, sed, awk, sort, uniq, head and wc so the output stays small; /tmp is scratch space. `cat /README.md` explains the layout. Nothing here changes Omni or sends anything. Pass the same `session` to keep /tmp, the working directory and variables between calls for 30 minutes.",
    annotations: Annotations {
        open_world_hint: true,
        ..READ_ONLY
    },
    policy: Policy {
        side_effects: &[
            "Runs Omni's read-only tools, which may read the configured mailbox, calendar and other connected accounts",
        ],
        cost: "No paid API; the read-only tools a script runs",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static SHELL_USAGE: ToolDef<ShellUsageInput, ShellUsageOutput> = ToolDef::new(ToolInfo {
    name: "shell_usage",
    title: "Measure Shell Use",
    description: "Summarize how the `shell` tool has been used compared with direct tool calls: runs, failures, durations, the tool data scripts read against the output they returned, which tools scripts used, recent failures, the result sizes of direct read-only tool calls, and the size of the served tool list against the shell tool's own definition. Use it to judge whether the shell saves context.",
    annotations: READ_ONLY,
    policy: Policy {
        side_effects: &[],
        cost: "none",
        recommended: ApprovalPolicy::Allow,
    },
});

/// Every shell tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 2] = [&SHELL, &SHELL_USAGE];

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellInput {
    /// The bash script to run.
    #[schemars(length(min = 1, max = 20000))]
    pub script: String,
    /// A name to keep /tmp, the working directory and variables between
    /// calls; sessions end after 30 idle minutes. Omit for a fresh shell.
    #[schemars(pattern("^[A-Za-z0-9_-]{1,64}$"))]
    pub session: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellOutput {
    /// The script's exit status; nonzero when its last command failed or a
    /// limit stopped it.
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    /// Whether stdout or stderr was cut to fit the response.
    pub truncated: bool,
    pub duration_ms: i64,
    /// The `shell_usage` record of this run.
    pub run_id: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellUsageInput {
    /// How many days back to summarize.
    #[schemars(range(min = 1, max = 90), extend("default" = 14))]
    pub days: Option<u32>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellUsageOutput {
    pub since: String,
    pub shell: ShellSummary,
    pub direct: DirectSummary,
    pub schemas: SchemaSizes,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShellSummary {
    pub runs: i64,
    pub sessions: i64,
    pub succeeded: i64,
    pub failed: i64,
    /// Runs whose request went away before they finished.
    pub interrupted: i64,
    /// Runs stopped by the timeout or another sandbox limit.
    pub limited: i64,
    pub truncated: i64,
    pub median_duration_ms: Option<i64>,
    pub p90_duration_ms: Option<i64>,
    pub tool_calls: i64,
    /// Bytes of tool results the scripts read, as the compact JSON a direct
    /// call returns.
    pub tool_bytes: i64,
    /// Bytes the runs returned (stdout plus stderr after truncation).
    pub returned_bytes: i64,
    /// `1 - returned / tool`, when any tool data was read.
    pub reduction: Option<f64>,
    pub by_tool: Vec<ToolCount>,
    pub recent_failures: Vec<RunFailure>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCount {
    pub tool: String,
    pub calls: i64,
    pub bytes: i64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunFailure {
    pub at: String,
    pub run_id: String,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
    /// The start of the script.
    pub script: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectSummary {
    /// Recorded calls of every tool except `shell` and `shell_usage` in the
    /// window (activity keeps the newest 2,000 calls).
    pub calls: i64,
    /// Calls that recorded their result size (calls before the measurement
    /// existed did not).
    pub measured: i64,
    pub result_bytes: i64,
    pub median_result_bytes: Option<i64>,
    pub shell_calls: i64,
    pub shell_result_bytes: i64,
    pub by_tool: Vec<DirectTool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DirectTool {
    pub tool: String,
    pub read_only: bool,
    pub calls: i64,
    pub result_bytes: i64,
    pub median_result_bytes: Option<i64>,
    pub max_result_bytes: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaSizes {
    pub served_tools: i64,
    /// Bytes of every served Omni tool's `tools/list` entry, integrations
    /// excluded.
    pub tools_list_bytes: i64,
    pub shell_tool_bytes: i64,
    pub shell_commands: i64,
}
