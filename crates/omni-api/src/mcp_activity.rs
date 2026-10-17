//! MCP activity: `GET /api/mcp/activity`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::Ms;

/// Lifecycle of one recorded MCP tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum McpCallStatus {
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "interrupted")]
    Interrupted,
}

impl McpCallStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "ok" => Some(Self::Ok),
            "error" => Some(Self::Error),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }

    /// Error and interrupted calls count as failures in summaries.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Interrupted)
    }
}

/// The recommended Executor policy of a tool (`docs/mcp-policy.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecommendedPolicy {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "require_approval")]
    RequireApproval,
    #[serde(rename = "block")]
    Block,
}

/// One recorded call (`McpCallData`). `input` and `output` are bounded,
/// secret-redacted JSON; `output` is kept only for `claude_*` tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCall {
    pub call_id: String,
    pub tool: String,
    pub title: String,
    pub recommended_policy: RecommendedPolicy,
    pub read_only: bool,
    pub started_at: Ms,
    pub finished_at: Option<Ms>,
    pub duration_ms: Option<Ms>,
    pub status: McpCallStatus,
    pub error: Option<String>,
    pub input: Value,
    pub output: Value,
}

/// Per-tool statistics, most recently used first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSummary {
    pub tool: String,
    pub title: String,
    pub calls: u64,
    pub errors: u64,
    pub last_at: Ms,
    pub avg_duration_ms: Option<i64>,
    pub recommended_policy: RecommendedPolicy,
}

/// Counts over the stored (and the last 24 hours of) calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpActivitySummary {
    pub stored: u64,
    pub last24h: u64,
    pub errors24h: u64,
    pub running: u64,
    pub approval_calls24h: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRetention {
    pub max_calls: u64,
}

/// `GET /api/mcp/activity?limit&tool&status&before`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpActivityResponse {
    pub calls: Vec<McpCall>,
    /// `before` for the next page (the last call's `startedAt`).
    pub next_before: Option<Ms>,
    pub summary: McpActivitySummary,
    pub tools: Vec<McpToolSummary>,
    pub retention: McpRetention,
}

pub mod paths {
    pub const MCP_ACTIVITY: &str = "/api/mcp/activity";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn calls_round_trip_with_explicit_nulls() {
        let call = json!({
            "callId": "c",
            "tool": "claude_session_start",
            "title": "Start",
            "recommendedPolicy": "require_approval",
            "readOnly": false,
            "startedAt": 1,
            "finishedAt": null,
            "durationMs": null,
            "status": "running",
            "error": null,
            "input": {"project": "omni"},
            "output": null
        });
        let parsed: McpCall = serde_json::from_value(call.clone()).unwrap();
        assert_eq!(parsed.status, McpCallStatus::Running);
        assert_eq!(serde_json::to_value(parsed).unwrap(), call);
    }
}
