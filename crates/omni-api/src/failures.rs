//! `GET /api/failures`: what is failing across Omni, newest first: task
//! runs whose last run failed or was degraded, failed and invalid tool
//! calls, dead event deliveries and integrations in error.

use serde::{Deserialize, Serialize};

use crate::common::Ms;

pub const PATH: &str = "/api/failures";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureKind {
    #[serde(rename = "task")]
    Task,
    #[serde(rename = "tool_call")]
    ToolCall,
    #[serde(rename = "delivery")]
    Delivery,
    #[serde(rename = "integration")]
    Integration,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureItem {
    pub kind: FailureKind,
    /// The task, tool, event or integration.
    pub title: String,
    /// The error, bounded; a tool call's caller follows it.
    pub detail: String,
    pub at: Ms,
    /// Where the full story is (`/operations`, `/mcp-activity`, `/gateway`).
    pub link: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureCounts {
    pub tasks: u32,
    pub tool_calls: u32,
    pub deliveries: u32,
    pub integrations: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailuresResponse {
    pub checked_at: Ms,
    /// Tool calls and deliveries from the last 7 days; tasks and
    /// integrations as they stand now. At most 200, newest first.
    pub items: Vec<FailureItem>,
    pub counts: FailureCounts,
}
