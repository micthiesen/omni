//! The Claude Code session tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use omni_mcp_kit::schema::lead_description;
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static CLAUDE_LINK_STATUS: ToolDef<ClaudeLinkStatusInput, ClaudeLinkStatusOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_link_status",
        title: "Get Claude Code Host Status",
        description: "Report whether the Claude Code host is connected to Omni for session control, whether its kill switch is engaged, when it last checked in, and the projects where new sessions may start (the same list its Remote Control servers use; null while the host is unreachable). Start sessions by project name.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &["Reads Omni's link state and the host's project configuration"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static CLAUDE_SESSIONS_LIST: ToolDef<ClaudeSessionsListInput, ClaudeSessionsListOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_sessions_list",
        title: "List Claude Code Sessions",
        description: "List Claude Code sessions on the Claude Code host, newest first: background, interactive terminal, and Remote Control sessions in any directory. Each session reports its configured project when its directory belongs to one. Filter by project name to narrow the list.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &["Reads session metadata and transcripts on the host"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static CLAUDE_SESSION_GET: ToolDef<ClaudeSessionGetInput, ClaudeSessionGetOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_session_get",
        title: "Get Claude Code Session",
        description: "Read one Claude Code session's status, transcript revision, and last assistant text. To wait for a turn to finish, pass waitSeconds (up to 45) with afterRevision from a start or send result so the wait cannot return before the new turn; when timedOut is true, call again. includeResult adds all assistant text since the last user input once the turn has finished. Subscribing to the claude.session.turn_finished MCP event avoids repeated waits where the client supports MCP Events.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &["Reads session status and transcripts on the host"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static CLAUDE_SESSION_READ: ToolDef<ClaudeSessionReadInput, ClaudeSessionReadOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_session_read",
        title: "Read Claude Code Transcript",
        description: "Page through a Claude Code session's transcript: user and assistant text, tool calls, and abbreviated tool results. Without a cursor it returns the latest items; pass nextCursor to continue forward.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &["Reads a session transcript on the host"],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static CLAUDE_SESSION_START: ToolDef<ClaudeSessionStartInput, ClaudeSessionStartOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_session_start",
        title: "Start Claude Code Session",
        description: "Start a new background Claude Code session on the Claude Code host in a configured project (see claude_link_status). The session runs with full access (bypass permissions) and can edit, run commands, commit, and push in that project. Reusing an idempotencyKey for the same project returns the existing session instead of starting another.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Starts a Claude Code process on the host with full access to the project",
                "The agent may edit files, run commands, commit, push, and use the network",
            ],
            cost: "Consumes Claude subscription usage",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static CLAUDE_SESSION_SEND: ToolDef<ClaudeSessionSendInput, ClaudeSessionSendOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_session_send",
        title: "Send Input to Claude Code Session",
        description: "Continue an idle background Claude Code session with new user input. Busy sessions are refused unless interrupt is true, which ends the current turn first. Interactive terminal sessions cannot receive input here. Pass an idempotencyKey so a retry returns the earlier send (reused) instead of sending twice; a retry whose earlier attempt never finished fails with send_in_doubt. Without a key, read the session before sending again after an unknown result.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: true,
            idempotent_hint: false,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Resumes a Claude Code session on the host with full access to its directory",
                "The agent may edit files, run commands, commit, push, and use the network",
            ],
            cost: "Consumes Claude subscription usage",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static CLAUDE_SESSION_STOP: ToolDef<ClaudeSessionStopInput, ClaudeSessionStopOutput> =
    ToolDef::new(ToolInfo {
        name: "claude_session_stop",
        title: "Stop Claude Code Session",
        description: "Stop a background Claude Code session on the Claude Code host. The conversation is kept and can be continued later with claude_session_send.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &["Ends the session's running process and any turn in progress"],
            cost: "none",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

/// Every Claude Code session tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 7] = [
    &CLAUDE_LINK_STATUS,
    &CLAUDE_SESSIONS_LIST,
    &CLAUDE_SESSION_GET,
    &CLAUDE_SESSION_READ,
    &CLAUDE_SESSION_START,
    &CLAUDE_SESSION_SEND,
    &CLAUDE_SESSION_STOP,
];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClaudeLinkStatusInput {}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub exists: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeLinkStatusOutput {
    pub configured: bool,
    pub online: bool,
    pub disabled: bool,
    pub last_seen_at: Option<String>,
    pub pending_jobs: u64,
    pub projects: Option<Vec<Project>>,
    pub projects_error: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionsListInput {
    #[schemars(
        description = "Project name from claude_projects_list",
        length(max = 100),
        pattern("^[A-Za-z0-9][A-Za-z0-9._-]*$")
    )]
    pub project: Option<String>,
    #[schemars(extend("default" = false))]
    pub include_stopped: Option<bool>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
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
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClaudeSessionsListOutput {
    pub sessions: Vec<Session>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionGetInput {
    #[schemars(
        description = "Short id from claude_sessions_list, a full session id, or a unique prefix",
        pattern("^[0-9A-Za-z-]{4,64}$")
    )]
    pub session: String,
    pub after_revision: Option<u64>,
    #[schemars(range(max = 45), extend("default" = 0))]
    pub wait_seconds: Option<u64>,
    #[schemars(extend("default" = false))]
    pub include_result: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionGetOutput {
    pub session: Session,
    pub timed_out: bool,
    pub result: Option<String>,
    pub truncated: bool,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClaudeSessionReadInput {
    #[schemars(
        description = "Short id from claude_sessions_list, a full session id, or a unique prefix",
        pattern("^[0-9A-Za-z-]{4,64}$")
    )]
    pub session: String,
    #[schemars(range(min = 1, max = 100), extend("default" = 20))]
    pub limit: Option<u64>,
    pub cursor: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionReadItem {
    pub index: u64,
    pub kind: String,
    pub timestamp: Option<String>,
    pub text: Option<String>,
    pub truncated: bool,
    pub tool: Option<String>,
    pub input: Option<String>,
    pub is_error: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionReadOutput {
    pub session_id: String,
    pub items: Vec<ClaudeSessionReadItem>,
    pub revision: u64,
    pub next_cursor: Option<i64>,
    pub has_more: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionStartInput {
    #[schemars(
        description = "Project name from claude_projects_list",
        length(max = 100),
        pattern("^[A-Za-z0-9][A-Za-z0-9._-]*$")
    )]
    pub project: String,
    #[schemars(length(min = 1, max = 100000))]
    pub prompt: String,
    #[schemars(
        description = "Caller-chosen key that makes retries safe",
        pattern("^[A-Za-z0-9._:-]{1,128}$")
    )]
    pub idempotency_key: String,
    #[schemars(length(min = 1, max = 200))]
    pub title: Option<String>,
    #[schemars(description = "Claude Code model alias or id, such as sonnet or opus", pattern(r#"^[A-Za-z0-9._[\]-]{1,64}$"#), transform = lead_description)]
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClaudeSessionStartOutput {
    pub session: Session,
    pub reused: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionSendInput {
    #[schemars(
        description = "Short id from claude_sessions_list, a full session id, or a unique prefix",
        pattern("^[0-9A-Za-z-]{4,64}$")
    )]
    pub session: String,
    #[schemars(length(min = 1, max = 100000))]
    pub prompt: String,
    #[schemars(extend("default" = false))]
    pub interrupt: Option<bool>,
    #[schemars(description = "Caller-chosen key that makes retries safe", pattern("^[A-Za-z0-9._:-]{1,128}$"), transform = lead_description)]
    pub idempotency_key: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionSendOutput {
    pub session: Session,
    pub previous_revision: u64,
    pub reused: bool,
    pub warning: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ClaudeSessionStopInput {
    #[schemars(
        description = "Short id from claude_sessions_list, a full session id, or a unique prefix",
        pattern("^[0-9A-Za-z-]{4,64}$")
    )]
    pub session: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeSessionStopOutput {
    pub id: Option<String>,
    pub session_id: String,
}
