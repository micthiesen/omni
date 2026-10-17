//! Grouping and summaries of `claude_*` MCP calls.

use std::collections::HashMap;

use omni_api::mcp_activity::{McpCall, McpCallStatus};
use serde_json::{Map, Value};

use super::js::{parse_date_ms, utf16_len, utf16_slice};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClaudeActionKind {
    Start,
    Send,
    Wait,
    Result,
    Get,
    Read,
    Stop,
    List,
    Projects,
    LinkStatus,
    Other,
}

impl ClaudeActionKind {
    fn from_tool(tool: &str) -> Self {
        match tool {
            "claude_session_start" => Self::Start,
            "claude_session_send" => Self::Send,
            "claude_session_wait" => Self::Wait,
            "claude_session_result" => Self::Result,
            "claude_session_get" => Self::Get,
            "claude_session_read" => Self::Read,
            "claude_session_stop" => Self::Stop,
            "claude_sessions_list" => Self::List,
            "claude_projects_list" => Self::Projects,
            "claude_link_status" => Self::LinkStatus,
            _ => Self::Other,
        }
    }

    /// Tools that act on no particular session render in the "Other" group.
    pub fn is_sessionless(self) -> bool {
        matches!(
            self,
            Self::List | Self::Projects | Self::LinkStatus | Self::Other
        )
    }

    /// `ACTION_LABELS`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Started",
            Self::Send => "Sent",
            Self::Wait => "Waited",
            Self::Result => "Result",
            Self::Get => "Checked",
            Self::Read => "Read transcript",
            Self::Stop => "Stopped",
            Self::List => "Listed sessions",
            Self::Projects => "Listed projects",
            Self::LinkStatus => "Link status",
            Self::Other => "Call",
        }
    }

    /// The wire value (`"link_status"` etc.), used as a CSS suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Send => "send",
            Self::Wait => "wait",
            Self::Result => "result",
            Self::Get => "get",
            Self::Read => "read",
            Self::Stop => "stop",
            Self::List => "list",
            Self::Projects => "projects",
            Self::LinkStatus => "link_status",
            Self::Other => "other",
        }
    }
}

pub fn is_claude_tool(tool: &str) -> bool {
    tool.starts_with("claude_")
}

/// `claude_session_get` also waits and returns results; older calls used
/// separate wait/result tools, which stay mapped for history.
pub fn claude_action_kind(tool: &str, input: &Value) -> ClaudeActionKind {
    let kind = ClaudeActionKind::from_tool(tool);
    if kind != ClaudeActionKind::Get {
        return kind;
    }
    let input = as_record(input);
    if input.and_then(|r| r.get("includeResult")) == Some(&Value::Bool(true)) {
        return ClaudeActionKind::Result;
    }
    match input
        .and_then(|r| r.get("waitSeconds"))
        .and_then(Value::as_f64)
    {
        Some(wait) if wait > 0.0 => ClaudeActionKind::Wait,
        _ => ClaudeActionKind::Get,
    }
}

pub fn as_record(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

pub fn string_field(value: &Value, key: &str) -> Option<String> {
    match as_record(value)?.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

pub fn number_field(value: &Value, key: &str) -> Option<f64> {
    as_record(value)?
        .get(key)?
        .as_f64()
        .filter(|n| n.is_finite())
}

pub fn boolean_field(value: &Value, key: &str) -> Option<bool> {
    as_record(value)?.get(key)?.as_bool()
}

pub fn array_field<'a>(value: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    as_record(value)?.get(key)?.as_array()
}

/// The session snapshot an action's output carries, when it has one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSnapshot {
    pub session_id: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub project: Option<String>,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub revision: Option<f64>,
}

pub fn output_session(call: &McpCall) -> Option<SessionSnapshot> {
    let session = as_record(&call.output)?.get("session")?;
    as_record(session)?;
    Some(SessionSnapshot {
        session_id: string_field(session, "sessionId"),
        id: string_field(session, "id"),
        title: string_field(session, "title"),
        project: string_field(session, "project"),
        status: string_field(session, "status"),
        kind: string_field(session, "kind"),
        revision: number_field(session, "revision"),
    })
}

/// Full session id recorded in the output (`session.sessionId`, else `sessionId`).
pub fn output_session_id(call: &McpCall) -> Option<String> {
    output_session(call)
        .and_then(|s| s.session_id)
        .or_else(|| string_field(&call.output, "sessionId"))
}

/// The session reference the caller passed: short id, prefix, or UUID.
pub fn input_session_ref(call: &McpCall) -> Option<String> {
    string_field(&call.input, "session")
}

pub fn short_session_id(session_id: &str) -> String {
    utf16_slice(session_id, 8)
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeActionGroup {
    pub key: String,
    /// Full session id when known; otherwise the reference the caller used.
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub project: Option<String>,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub latest_at: i64,
    /// Oldest first, so the timeline reads as a conversation.
    pub actions: Vec<McpCall>,
    pub latest_failed: bool,
}

struct KnownSession {
    session_id: String,
    id: Option<String>,
}

fn resolve_ref(reference: &str, known: &[KnownSession]) -> String {
    if let Some(exact) = known
        .iter()
        .find(|k| k.session_id == reference || k.id.as_deref() == Some(reference))
    {
        return exact.session_id.clone();
    }
    let prefixed: Vec<&KnownSession> = known
        .iter()
        .filter(|k| {
            k.session_id.starts_with(reference)
                || k.id.as_deref().is_some_and(|id| id.starts_with(reference))
        })
        .collect();
    match prefixed.as_slice() {
        [only] => only.session_id.clone(),
        _ => reference.to_owned(),
    }
}

/// Grouped `claude_*` calls: per-session groups (newest first) and the rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupedClaudeActions {
    pub sessions: Vec<ClaudeActionGroup>,
    pub other: Vec<McpCall>,
}

/// Group calls by the session they acted on. Output session ids win; calls
/// without output resolve their input reference against ids seen elsewhere
/// so a short id joins its full-id group. Input is newest first.
pub fn group_claude_actions(actions: &[McpCall]) -> GroupedClaudeActions {
    let mut known: Vec<KnownSession> = Vec::new();
    for call in actions {
        let session = output_session(call);
        let Some(session_id) = output_session_id(call) else {
            continue;
        };
        if known.iter().any(|k| k.session_id == session_id) {
            continue;
        }
        known.push(KnownSession {
            session_id,
            id: session
                .and_then(|s| s.id)
                .or_else(|| string_field(&call.output, "id")),
        });
    }

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, ClaudeActionGroup> = HashMap::new();
    let mut other = Vec::new();
    for call in actions {
        let kind = claude_action_kind(&call.tool, &call.input);
        if kind.is_sessionless() {
            other.push(call.clone());
            continue;
        }
        let reference = input_session_ref(call);
        let full_id = output_session_id(call)
            .or_else(|| reference.as_deref().map(|r| resolve_ref(r, &known)));
        let key = full_id
            .clone()
            .unwrap_or_else(|| format!("call:{}", call.call_id));
        let group = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            ClaudeActionGroup {
                key: key.clone(),
                session_id: full_id.clone(),
                title: None,
                project: None,
                status: None,
                kind: None,
                latest_at: call.started_at,
                actions: Vec::new(),
                latest_failed: call.status == McpCallStatus::Error,
            }
        });
        group.actions.push(call.clone());
        group.latest_at = group.latest_at.max(call.started_at);
        let session = output_session(call).unwrap_or_default();
        if group.title.is_none() {
            group.title = session.title;
        }
        if group.project.is_none() {
            group.project = session.project;
        }
        if group.status.is_none() {
            group.status = session.status;
        }
        if group.kind.is_none() {
            group.kind = session.kind;
        }
        if kind == ClaudeActionKind::Start {
            if group.title.is_none() {
                group.title = string_field(&call.input, "title");
            }
            if group.project.is_none() {
                group.project = string_field(&call.input, "project");
            }
        }
    }

    let mut sessions: Vec<ClaudeActionGroup> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect();
    for group in &mut sessions {
        group.actions.sort_by_key(|a| a.started_at);
    }
    sessions.sort_by_key(|g| std::cmp::Reverse(g.latest_at));
    GroupedClaudeActions { sessions, other }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorExplanation {
    pub code: Option<String>,
    pub hint: Option<String>,
}

const ERROR_HINTS: [(&str, &str); 5] = [
    (
        "outcome_unknown",
        "The Mac may have run this. Check the session before retrying.",
    ),
    (
        "not_picked_up",
        "The Mac never claimed the job before it expired, so it did not run.",
    ),
    (
        "not_configured",
        "The Mac link is not configured on the server.",
    ),
    (
        "disabled",
        "The link is switched off on the Mac. Run `omni-link enable` there to resume.",
    ),
    (
        "offline",
        "The Mac is not polling: it may be asleep, away without VPN, or the agent stopped.",
    ),
];

/// `/\(([a-z][a-z_]*)\)\s*$/` on the trimmed message.
fn trailing_code(message: &str) -> Option<&str> {
    let trimmed = message.trim();
    let inner = trimmed.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let code = &inner[open + 1..];
    let mut chars = code.chars();
    let first = chars.next()?;
    if first.is_ascii_lowercase() && chars.all(|c| c.is_ascii_lowercase() || c == '_') {
        Some(code)
    } else {
        None
    }
}

/// Find a link failure code in an error message and explain the known ones.
pub fn explain_claude_error(error: Option<&str>) -> ErrorExplanation {
    let none = ErrorExplanation {
        code: None,
        hint: None,
    };
    let Some(error) = error.filter(|e| !e.is_empty()) else {
        return none;
    };
    if let Some(code) = trailing_code(error) {
        let hint = ERROR_HINTS
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, h)| (*h).to_owned());
        return ErrorExplanation {
            code: Some(code.to_owned()),
            hint,
        };
    }
    let lowered = error.to_lowercase();
    for (code, hint) in ERROR_HINTS {
        if lowered.contains(code) || lowered.contains(&code.replace('_', " ")) {
            return ErrorExplanation {
                code: Some(code.to_owned()),
                hint: Some(hint.to_owned()),
            };
        }
    }
    none
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// One-line description for compact (non-conversational) actions.
pub fn summarize_compact_action(call: &McpCall) -> String {
    let kind = claude_action_kind(&call.tool, &call.input);
    let output = &call.output;
    match kind {
        ClaudeActionKind::List => {
            let scope = string_field(&call.input, "project")
                .map(|p| format!(" in {p}"))
                .unwrap_or_default();
            match array_field(output, "sessions") {
                Some(sessions) => {
                    format!("{}{scope}", plural(sessions.len(), "session", "sessions"))
                }
                None => format!("Sessions{scope}"),
            }
        }
        ClaudeActionKind::Projects => match array_field(output, "projects") {
            Some(projects) => plural(projects.len(), "project", "projects"),
            None => "Projects".to_owned(),
        },
        ClaudeActionKind::LinkStatus => {
            if as_record(output).is_none() {
                return "Link status".to_owned();
            }
            let state = if boolean_field(output, "disabled") == Some(true) {
                "disabled"
            } else if boolean_field(output, "online") == Some(true) {
                "online"
            } else {
                "offline"
            };
            match string_field(output, "host") {
                Some(host) => format!("{state} · {host}"),
                None => state.to_owned(),
            }
        }
        ClaudeActionKind::Read => {
            let Some(items) = array_field(output, "items") else {
                return "Transcript".to_owned();
            };
            let more = if boolean_field(output, "hasMore") == Some(true) {
                ", more available"
            } else {
                ""
            };
            format!("{}{more}", plural(items.len(), "item", "items"))
        }
        ClaudeActionKind::Get => {
            let Some(session) = output_session(call) else {
                return "Session".to_owned();
            };
            match session.revision {
                None => session.status.unwrap_or_else(|| "Session".to_owned()),
                Some(revision) => format!(
                    "{} · rev {}",
                    session.status.as_deref().unwrap_or("unknown"),
                    super::js::number_string(revision)
                ),
            }
        }
        other => other.label().to_owned(),
    }
}

pub fn parse_iso_ms(iso: Option<&str>) -> Option<f64> {
    parse_date_ms(iso.filter(|s| !s.is_empty())?)
}

/// Strings as-is; anything else as `JSON.stringify(value, null, 2)`.
pub fn format_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

const TOOL_INPUT_KEYS: [&str; 11] = [
    "command",
    "file_path",
    "pattern",
    "url",
    "query",
    "description",
    "prompt",
    "skill",
    "code",
    "summary",
    "message",
];

/// One readable line for a transcript snippet, cut at `max` UTF-16 units.
pub fn snippet(text: Option<&str>, max: usize) -> Option<String> {
    let line = text?
        .split('\n')
        .map(|part| part.trim())
        .find(|part| !part.is_empty())?;
    if utf16_len(line) > max {
        Some(format!("{}…", utf16_slice(line, max.saturating_sub(1))))
    } else {
        Some(line.to_owned())
    }
}

/// The meaningful part of a tool call's JSON input, such as a Bash command.
pub fn tool_input_summary(input: Option<&str>) -> Option<String> {
    let input = input.filter(|s| !s.is_empty())?;
    match serde_json::from_str::<Value>(input) {
        Ok(value) => {
            if let Some(record) = value.as_object() {
                for key in TOOL_INPUT_KEYS {
                    if let Some(Value::String(text)) = record.get(key) {
                        return snippet(Some(text), 140);
                    }
                }
            }
        }
        Err(_) => {
            // Inputs are capped at 1,000 characters, so long JSON arrives cut off.
            if let Some(found) = truncated_key_value(input) {
                return snippet(Some(&found.replace("\\n", "\n").replace("\\\"", "\"")), 140);
            }
        }
    }
    snippet(Some(input), 140)
}

/// `/"(?:command|...|message)":"((?:[^"\\]|\\.)*)/` first match, group 1.
fn truncated_key_value(input: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for key in TOOL_INPUT_KEYS {
        let needle = format!("\"{key}\":\"");
        let Some(start) = input.find(&needle) else {
            continue;
        };
        if best.as_ref().is_some_and(|(at, _)| *at <= start) {
            continue;
        }
        let rest = &input[start + needle.len()..];
        let mut captured = String::new();
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => break,
                '\\' => match chars.next() {
                    Some(next) => {
                        captured.push('\\');
                        captured.push(next);
                    }
                    None => break,
                },
                other => captured.push(other),
            }
        }
        best = Some((start, captured));
    }
    best.map(|(_, captured)| captured).filter(|c| !c.is_empty())
}

#[cfg(test)]
mod tests {
    use omni_api::mcp_activity::RecommendedPolicy;
    use serde_json::json;

    use super::*;

    const FULL_ID: &str = "25ffb449-1111-4222-8333-444455556666";

    fn call(call_id: &str, tool: &str) -> McpCall {
        McpCall {
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            title: tool.to_owned(),
            recommended_policy: RecommendedPolicy::Allow,
            read_only: false,
            started_at: 0,
            finished_at: None,
            duration_ms: None,
            status: McpCallStatus::Ok,
            error: None,
            input: json!({}),
            output: Value::Null,
        }
    }

    fn with(
        mut base: McpCall,
        started_at: i64,
        status: McpCallStatus,
        input: Value,
        output: Value,
    ) -> McpCall {
        base.started_at = started_at;
        base.status = status;
        base.input = input;
        base.output = output;
        base
    }

    fn session(status: &str, revision: i64) -> Value {
        json!({
            "id": "25ffb449",
            "sessionId": FULL_ID,
            "kind": "background",
            "title": "Fix the build",
            "cwd": "/Users/michael/Code/omni",
            "project": "omni",
            "status": status,
            "state": null,
            "startedAt": null,
            "revision": revision,
            "lastAssistant": null,
        })
    }

    #[test]
    fn joins_short_id_calls_without_output_to_the_full_session_group() {
        let mut c4 = with(
            call("c4", "claude_session_send"),
            40,
            McpCallStatus::Error,
            json!({"session": "25ffb449", "prompt": "again"}),
            Value::Null,
        );
        c4.error = Some("offline".to_owned());
        let actions = vec![
            c4,
            with(
                call("c3", "claude_link_status"),
                30,
                McpCallStatus::Ok,
                json!({}),
                json!({"online": true, "disabled": false, "host": "mbp"}),
            ),
            with(
                call("c2", "claude_session_wait"),
                20,
                McpCallStatus::Ok,
                json!({"session": "25ff"}),
                json!({"session": session("idle", 3), "timedOut": false}),
            ),
            with(
                call("c1", "claude_session_start"),
                10,
                McpCallStatus::Ok,
                json!({"project": "omni", "prompt": "Fix it"}),
                json!({"session": session("busy", 1), "reused": false}),
            ),
        ];
        let grouped = group_claude_actions(&actions);
        assert_eq!(
            grouped
                .other
                .iter()
                .map(|c| c.call_id.as_str())
                .collect::<Vec<_>>(),
            ["c3"]
        );
        assert_eq!(grouped.sessions.len(), 1);
        let group = &grouped.sessions[0];
        assert_eq!(group.session_id.as_deref(), Some(FULL_ID));
        assert_eq!(
            group
                .actions
                .iter()
                .map(|c| c.call_id.as_str())
                .collect::<Vec<_>>(),
            ["c1", "c2", "c4"]
        );
        assert_eq!(group.status.as_deref(), Some("idle"));
        assert_eq!(group.project.as_deref(), Some("omni"));
        assert_eq!(group.latest_at, 40);
        assert!(group.latest_failed);
    }

    #[test]
    fn keeps_a_failed_start_without_a_session_in_its_own_group_newest_first() {
        let mut late = with(
            call("late", "claude_session_start"),
            50,
            McpCallStatus::Error,
            json!({"project": "dotfiles", "prompt": "x", "title": "Tidy"}),
            Value::Null,
        );
        late.error = Some("disabled".to_owned());
        let early = with(
            call("early", "claude_session_stop"),
            5,
            McpCallStatus::Ok,
            json!({"session": "abc"}),
            json!({"id": "abc", "sessionId": "abc-full"}),
        );
        let grouped = group_claude_actions(&[late, early]);
        assert_eq!(
            grouped
                .sessions
                .iter()
                .map(|g| g.key.as_str())
                .collect::<Vec<_>>(),
            ["call:late", "abc-full"]
        );
        assert_eq!(grouped.sessions[0].title.as_deref(), Some("Tidy"));
        assert_eq!(grouped.sessions[0].project.as_deref(), Some("dotfiles"));
    }

    fn code(message: &str) -> Option<String> {
        explain_claude_error(Some(message)).code
    }

    #[test]
    fn recognizes_link_failure_codes_in_messages() {
        assert_eq!(
            code("Job outcome_unknown after timeout").as_deref(),
            Some("outcome_unknown")
        );
        assert_eq!(code("The Mac link is offline").as_deref(), Some("offline"));
        assert_eq!(
            code("job was not picked up").as_deref(),
            Some("not_picked_up")
        );
        assert_eq!(
            explain_claude_error(Some("boom")),
            ErrorExplanation {
                code: None,
                hint: None
            }
        );
    }

    #[test]
    fn prefers_the_trailing_code_the_device_link_appends() {
        assert_eq!(
            code("The Mac is offline (last seen 5m ago) (offline)").as_deref(),
            Some("offline")
        );
        assert_eq!(
            code("Mac did a thing; offline? (outcome_unknown)").as_deref(),
            Some("outcome_unknown")
        );
        assert_eq!(
            explain_claude_error(Some("No such session (not_found)")),
            ErrorExplanation {
                code: Some("not_found".to_owned()),
                hint: None
            }
        );
        assert_eq!(code("disabled").as_deref(), Some("disabled"));
    }

    #[test]
    fn summarizes_listings_and_reads() {
        let list = with(
            call("l", "claude_sessions_list"),
            0,
            McpCallStatus::Ok,
            json!({"project": "omni"}),
            json!({"sessions": [{}, {}]}),
        );
        assert_eq!(summarize_compact_action(&list), "2 sessions in omni");
        let read = with(
            call("r", "claude_session_read"),
            0,
            McpCallStatus::Ok,
            json!({}),
            json!({"items": [{}], "hasMore": true}),
        );
        assert_eq!(summarize_compact_action(&read), "1 item, more available");
        let status = with(
            call("s", "claude_link_status"),
            0,
            McpCallStatus::Ok,
            json!({}),
            json!({"online": false, "disabled": true, "host": "mbp"}),
        );
        assert_eq!(summarize_compact_action(&status), "disabled · mbp");
    }

    #[test]
    fn summarizes_tool_inputs_by_their_meaningful_field() {
        assert_eq!(
            tool_input_summary(Some(r#"{"command":"pnpm test\nmore","description":"x"}"#))
                .as_deref(),
            Some("pnpm test")
        );
        assert_eq!(
            tool_input_summary(Some(r#"{"file_path":"/a/b.ts"}"#)).as_deref(),
            Some("/a/b.ts")
        );
        assert_eq!(
            tool_input_summary(Some(r#"{"code":"const o = tools.omni; return o"#)).as_deref(),
            Some("const o = tools.omni; return o")
        );
        assert_eq!(tool_input_summary(None), None);
    }

    #[test]
    fn takes_the_first_non_empty_line_and_caps_its_length() {
        assert_eq!(
            snippet(Some("\n\n  hello\nworld"), 140).as_deref(),
            Some("hello")
        );
        assert_eq!(snippet(Some(&"x".repeat(10)), 5).as_deref(), Some("xxxx…"));
        assert_eq!(snippet(Some("   "), 140), None);
    }

    #[test]
    fn prefers_a_message_summary_over_raw_json() {
        assert_eq!(
            tool_input_summary(Some(
                r#"{"to":"agent","summary":"Re-verify fixes","message":"x"}"#
            ))
            .as_deref(),
            Some("Re-verify fixes")
        );
    }

    #[test]
    fn reads_waits_and_results_from_claude_session_get_input() {
        let get = |input: Value| claude_action_kind("claude_session_get", &input);
        assert_eq!(get(json!({"session": "abc"})), ClaudeActionKind::Get);
        assert_eq!(
            get(json!({"session": "abc", "waitSeconds": 30})),
            ClaudeActionKind::Wait
        );
        assert_eq!(
            get(json!({"session": "abc", "waitSeconds": 30, "includeResult": true})),
            ClaudeActionKind::Result
        );
        assert_eq!(
            claude_action_kind("claude_session_wait", &json!({})),
            ClaudeActionKind::Wait
        );
    }
}
