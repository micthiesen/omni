//! Claude Code host observability: `/api/claude/*`.
//!
//! These read-only routes are for Omni's own UI, which may show the host name.

use serde::{Deserialize, Serialize};

use crate::mcp_activity::{McpCall, McpRetention};

/// The device link state (`DeviceLinkStatus`, or the unconfigured default).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeLinkView {
    pub configured: bool,
    pub online: bool,
    pub disabled: bool,
    pub host: Option<String>,
    pub last_seen_at: Option<String>,
    pub pending_jobs: u64,
}

/// `GET /api/claude/activity?limit`: every `claude_*` call, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeActivityResponse {
    pub link: ClaudeLinkView,
    pub actions: Vec<McpCall>,
    pub retention: McpRetention,
}

/// One session summary in the MCP shape (`toSession`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSession {
    pub id: Option<String>,
    pub session_id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub status: String,
    pub state: Option<String>,
    pub started_at: Option<String>,
    pub revision: u64,
    pub last_assistant: Option<String>,
    /// What a `waiting` session's turn is paused on (`sandbox request`,
    /// `dialog open`, ...); `None` otherwise or when the host does not say.
    #[serde(default)]
    pub waiting_for: Option<String>,
}

/// `GET /api/claude/sessions?includeStopped&limit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeSessionsResponse {
    pub sessions: Vec<ClaudeSession>,
}

/// One transcript item (`toItem`): text capped at 8,000 characters, tool input
/// as JSON capped at 1,000.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTranscriptItem {
    pub index: u64,
    pub kind: String,
    pub timestamp: Option<String>,
    pub text: Option<String>,
    pub truncated: bool,
    pub tool: Option<String>,
    pub input: Option<String>,
    pub is_error: Option<bool>,
}

/// `GET /api/claude/sessions/:session/transcript?limit&cursor`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeTranscriptResponse {
    pub session_id: String,
    pub items: Vec<ClaudeTranscriptItem>,
    pub revision: u64,
    pub next_cursor: Option<i64>,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeProject {
    pub name: String,
    pub path: String,
    pub exists: bool,
}

/// `GET /api/claude/projects`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeProjectsResponse {
    pub projects: Vec<ClaudeProject>,
}

/// A live query that could not reach the host: 503 for `offline`, `disabled`,
/// `not_picked_up` and `not_configured`, else 502; 400 `bad_request` for an
/// invalid session id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeLinkFailure {
    pub error: String,
    pub code: String,
}

pub mod paths {
    use crate::common::encode_uri_component;

    pub const ACTIVITY: &str = "/api/claude/activity";
    pub const SESSIONS: &str = "/api/claude/sessions";
    pub const PROJECTS: &str = "/api/claude/projects";

    pub fn transcript(session: &str) -> String {
        format!(
            "/api/claude/sessions/{}/transcript",
            encode_uri_component(session)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transcript_round_trips() {
        let body = json!({
            "sessionId": "abc",
            "items": [{
                "index": 1, "kind": "assistant", "timestamp": null, "text": "hi",
                "truncated": false, "tool": null, "input": null, "isError": null
            }],
            "revision": 3,
            "nextCursor": null,
            "hasMore": false
        });
        let parsed: ClaudeTranscriptResponse = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), body);
        assert_eq!(
            paths::transcript("a b"),
            "/api/claude/sessions/a%20b/transcript"
        );
    }
}
