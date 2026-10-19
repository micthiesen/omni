//! Claude Code session tools.
//!
//! MCP results describe a generic "Claude Code host": they never name the
//! machine or expose its home directory ([`scrub_host_details`]). Sessions run
//! with full access by design; approval happens in the connected client,
//! guided by the recommended policy of `start`, `send` and `stop`, never inside
//! the session, so a background session never stalls on a permission prompt.
//! Do not add a permission mode.

pub mod defs;

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_api::claude::{ClaudeSession, ClaudeTranscriptItem};
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, typed_tool};
use omni_runtime::ports::{ClaudeSessionNotifier, ClaudeTurnStarted};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::truncate;
use crate::host::{ClaudeHost, HostCommand, HostError};

const LOG: &str = "MCP:ClaudeSessions";
const ITEM_TEXT_LIMIT: usize = 8_000;
const RESULT_TEXT_LIMIT: usize = 20_000;
const QUICK: Duration = Duration::from_secs(60);
const LAUNCH: Duration = Duration::from_secs(150);
const NOT_CONFIGURED: &str = "The Claude Code host link is not configured";
const DISABLED_DETAIL: &str =
    "Session control is disabled on the Claude Code host (kill switch); nothing ran";

fn str_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// `int()`: a non-negative integer, else 0.
fn int_of(value: Option<&Value>) -> u64 {
    match value.and_then(Value::as_f64) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(n) if n.is_finite() && n.fract() == 0.0 && n >= 0.0 => n as u64,
        _ => 0,
    }
}

fn record(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// `records()`: array items as objects (non-objects become empty).
pub fn records(value: Option<&Value>) -> Vec<Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().map(record).collect())
        .unwrap_or_default()
}

/// One session-client summary in the MCP shape.
pub fn to_session(raw: &Map<String, Value>) -> ClaudeSession {
    let started_at = raw
        .get("started_at")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .map(|n| {
            #[allow(clippy::cast_possible_truncation)]
            omni_core::js::to_iso_string(n as i64)
        });
    ClaudeSession {
        id: str_of(raw.get("id")),
        session_id: str_of(raw.get("session_id")).unwrap_or_default(),
        kind: str_of(raw.get("kind")),
        title: str_of(raw.get("title")),
        cwd: str_of(raw.get("cwd")),
        project: str_of(raw.get("project")),
        status: str_of(raw.get("status")).unwrap_or_else(|| "unknown".to_owned()),
        state: str_of(raw.get("state")),
        started_at,
        revision: int_of(raw.get("revision")),
        last_assistant: str_of(raw.get("last_assistant")),
    }
}

/// One transcript item, text and tool input bounded.
pub fn to_item(raw: &Map<String, Value>) -> ClaudeTranscriptItem {
    let text = str_of(raw.get("text")).map(|text| truncate(&text, ITEM_TEXT_LIMIT));
    let input = raw
        .get("input")
        .map(|input| truncate(&omni_core::js::json_stringify(input), 1_000).0);
    ClaudeTranscriptItem {
        index: int_of(raw.get("index")),
        kind: str_of(raw.get("kind")).unwrap_or_else(|| "unknown".to_owned()),
        timestamp: str_of(raw.get("timestamp")),
        truncated: text.as_ref().is_some_and(|(_, cut)| *cut),
        text: text.map(|(text, _)| text),
        tool: str_of(raw.get("tool")),
        input,
        is_error: raw.get("is_error").and_then(Value::as_bool),
    }
}

fn is_js_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `/\/Users\/[^/\s"'`]+/g` replaced with `~`.
fn replace_home_directories(text: &str) -> String {
    const PREFIX: &str = "/Users/";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        let name_len: usize = after
            .chars()
            .take_while(|c| !(*c == '/' || c.is_whitespace() || matches!(c, '"' | '\'' | '`')))
            .map(char::len_utf8)
            .sum();
        if name_len == 0 {
            out.push_str(&rest[..at + PREFIX.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..at]);
        out.push('~');
        rest = &after[name_len..];
    }
    out.push_str(rest);
    out
}

/// `new RegExp("\\b" + escape(host) + "\\b", "gi")` replaced with `the host`.
fn replace_host_name(text: &str, host: &str) -> String {
    let needle: Vec<char> = host.chars().collect();
    let chars: Vec<char> = text.chars().collect();
    let boundary = |i: usize| {
        let before = i.checked_sub(1).map(|j| chars[j]).is_some_and(is_js_word);
        let after = chars.get(i).copied().is_some_and(is_js_word);
        before != after
    };
    let same = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let end = i + needle.len();
        if end <= chars.len()
            && boundary(i)
            && boundary(end)
            && chars[i..end].iter().zip(&needle).all(|(a, b)| same(*a, *b))
        {
            out.push_str("the host");
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Removes the host name and home directories from every
/// string in a result, transcript text included.
pub fn scrub_host_details(value: &Value, host: Option<&str>) -> Value {
    let host = host.filter(|h| h.chars().count() >= 3);
    match value {
        Value::String(text) => {
            let homeless = replace_home_directories(text);
            Value::String(match host {
                Some(host) => replace_host_name(&homeless, host),
                None => homeless,
            })
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| scrub_host_details(v, host)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, v)| (key.clone(), scrub_host_details(v, host)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Generic wording for the kill switch, scrubbed detail otherwise.
fn public_error(error: HostError, host: Option<&str>) -> HostError {
    let detail = if error.code == "disabled" {
        DISABLED_DETAIL.to_owned()
    } else {
        match scrub_host_details(&Value::String(error.detail.clone()), host) {
            Value::String(detail) => detail,
            _ => error.detail.clone(),
        }
    };
    HostError::new(error.code, detail, error.retryable)
}

/// What the Claude tools use.
#[derive(Clone)]
pub struct ClaudeDeps {
    pub host: Option<Arc<dyn ClaudeHost>>,
    /// The `claude.session.turn_finished` watcher, when MCP events are enabled.
    pub watcher: Option<Arc<dyn ClaudeSessionNotifier>>,
}

/// Arguments with absent values omitted.
fn args(entries: &[(&str, Option<Value>)]) -> Map<String, Value> {
    entries
        .iter()
        .filter_map(|(key, value)| value.clone().map(|v| ((*key).to_owned(), v)))
        .collect()
}

impl ClaudeDeps {
    /// `run()`: forwards one command, scrubbing the result and any error.
    async fn run(
        &self,
        command: HostCommand,
        arguments: Map<String, Value>,
        timeout: Duration,
    ) -> Result<Map<String, Value>, ToolError> {
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| ToolError::execute(NOT_CONFIGURED))?;
        let target = str_of(arguments.get("session"))
            .or_else(|| str_of(arguments.get("project")))
            .unwrap_or_else(|| "-".to_owned());
        let started = Instant::now();
        let name = host.status().host;
        match host.execute(command, arguments, timeout).await {
            Ok(data) => {
                tracing::info!(
                    target: LOG,
                    "claude {} {target} ok in {}ms",
                    command.as_str(),
                    started.elapsed().as_millis()
                );
                Ok(record(&scrub_host_details(
                    &Value::Object(data),
                    name.as_deref(),
                )))
            }
            Err(error) => {
                let error = public_error(error, name.as_deref());
                tracing::warn!(
                    target: LOG,
                    "claude {} {target} failed ({}) in {}ms",
                    command.as_str(),
                    error.code,
                    started.elapsed().as_millis()
                );
                Err(ToolError::execute(error.to_string()))
            }
        }
    }

    /// Lets turn_finished fire for a turn that ends before the watcher's next poll.
    async fn note_turn_started(&self, session: &ClaudeSession, revision: u64) {
        let Some(watcher) = &self.watcher else {
            return;
        };
        if session.session_id.is_empty() {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let turn = ClaudeTurnStarted {
            session_id: session.session_id.clone(),
            id: session.id.clone(),
            project: session.project.clone(),
            revision: revision as f64,
        };
        watcher.note_turn_started(&turn).await;
    }
}

fn session_json(session: &ClaudeSession) -> Value {
    serde_json::to_value(session).unwrap_or(Value::Null)
}

#[derive(Deserialize)]
struct EmptyInput {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListInput {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    include_stopped: bool,
    #[serde(default = "default_list_limit")]
    limit: u64,
}

fn default_list_limit() -> u64 {
    25
}

fn default_read_limit() -> u64 {
    20
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetInput {
    session: String,
    #[serde(default)]
    after_revision: Option<u64>,
    #[serde(default)]
    wait_seconds: u64,
    #[serde(default)]
    include_result: bool,
}

#[derive(Deserialize)]
struct ReadInput {
    session: String,
    #[serde(default = "default_read_limit")]
    limit: u64,
    #[serde(default)]
    cursor: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartInput {
    project: String,
    prompt: String,
    idempotency_key: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendInput {
    session: String,
    prompt: String,
    #[serde(default)]
    interrupt: bool,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Deserialize)]
struct StopInput {
    session: String,
}

pub fn claude_session_tools(deps: &ClaudeDeps) -> Result<Vec<McpTool>, ToolMetaError> {
    let link_status = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_LINK_STATUS,
            move |_: EmptyInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let Some(host) = &deps.host else {
                        return Ok::<_, ToolError>(json!({
                            "configured": false,
                            "online": false,
                            "disabled": false,
                            "lastSeenAt": null,
                            "pendingJobs": 0,
                            "projects": null,
                            "projectsError": NOT_CONFIGURED,
                        }));
                    };
                    let status = host.status();
                    let (projects, projects_error) = if status.online && !status.disabled {
                        match deps.run(HostCommand::Projects, Map::new(), QUICK).await {
                            Ok(data) => {
                                let projects: Vec<Value> = records(data.get("projects"))
                                .iter()
                                .map(|project| {
                                    json!({
                                        "name": str_of(project.get("name")).unwrap_or_default(),
                                        "path": str_of(project.get("path")).unwrap_or_default(),
                                        "exists": project.get("exists") == Some(&Value::Bool(true)),
                                    })
                                })
                                .collect();
                                (Value::Array(projects), Value::Null)
                            }
                            Err(error) => (Value::Null, Value::String(error.message)),
                        }
                    } else if status.disabled {
                        (
                            Value::Null,
                            json!("Session control is disabled on the Claude Code host"),
                        )
                    } else {
                        (Value::Null, json!("The Claude Code host is offline"))
                    };
                    Ok(json!({
                        "configured": true,
                        "online": status.online,
                        "disabled": status.disabled,
                        "lastSeenAt": status.last_seen_at,
                        "pendingJobs": status.pending_jobs,
                        "projects": projects,
                        "projectsError": projects_error,
                    }))
                }
            },
        )?
    };

    let list = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSIONS_LIST,
            move |input: ListInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let data = deps
                        .run(
                            HostCommand::List,
                            args(&[
                                ("project", input.project.map(Value::String)),
                                ("all", Some(Value::Bool(input.include_stopped))),
                                ("limit", Some(json!(input.limit))),
                            ]),
                            QUICK,
                        )
                        .await?;
                    let sessions: Vec<Value> = records(data.get("sessions"))
                        .iter()
                        .map(|raw| session_json(&to_session(raw)))
                        .collect();
                    Ok::<_, ToolError>(json!({ "sessions": sessions }))
                }
            },
        )?
    };

    let get = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSION_GET,
            move |input: GetInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let waited = if input.wait_seconds > 0 {
                        Some(
                            deps.run(
                                HostCommand::Wait,
                                args(&[
                                    ("session", Some(Value::String(input.session.clone()))),
                                    ("after", input.after_revision.map(|r| json!(r))),
                                    ("timeout", Some(json!(input.wait_seconds))),
                                ]),
                                Duration::from_secs(input.wait_seconds + 20),
                            )
                            .await?,
                        )
                    } else {
                        None
                    };
                    let timed_out = waited
                        .as_ref()
                        .is_some_and(|data| data.get("timed_out") == Some(&Value::Bool(true)));
                    if !input.include_result || timed_out {
                        let data = match waited {
                            Some(data) => data,
                            None => {
                                deps.run(
                                    HostCommand::Status,
                                    args(&[(
                                        "session",
                                        Some(Value::String(input.session.clone())),
                                    )]),
                                    QUICK,
                                )
                                .await?
                            }
                        };
                        return Ok::<_, ToolError>(json!({
                            "session": session_json(&to_session(&data)),
                            "timedOut": timed_out,
                            "result": null,
                            "truncated": false,
                        }));
                    }
                    let data = deps
                        .run(
                            HostCommand::Result,
                            args(&[("session", Some(Value::String(input.session.clone())))]),
                            QUICK,
                        )
                        .await?;
                    let bounded =
                        str_of(data.get("result")).map(|text| truncate(&text, RESULT_TEXT_LIMIT));
                    Ok(json!({
                        "session": session_json(&to_session(&data)),
                        "timedOut": false,
                        "result": bounded.as_ref().map(|(text, _)| text),
                        "truncated": bounded.as_ref().is_some_and(|(_, cut)| *cut),
                    }))
                }
            },
        )?
    };

    let read = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSION_READ,
            move |input: ReadInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let data = deps
                        .run(
                            HostCommand::Read,
                            args(&[
                                ("session", Some(Value::String(input.session.clone()))),
                                ("limit", Some(json!(input.limit))),
                                ("cursor", input.cursor.map(|c| json!(c))),
                            ]),
                            QUICK,
                        )
                        .await?;
                    let items: Vec<Value> = records(data.get("items"))
                        .iter()
                        .map(|raw| serde_json::to_value(to_item(raw)).unwrap_or(Value::Null))
                        .collect();
                    Ok::<_, ToolError>(json!({
                        "sessionId": str_of(data.get("session_id")).unwrap_or_default(),
                        "items": items,
                        "revision": int_of(data.get("revision")),
                        "nextCursor": data.get("next_cursor").filter(|v| v.is_number()),
                        "hasMore": data.get("has_more") == Some(&Value::Bool(true)),
                    }))
                }
            },
        )?
    };

    let start = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSION_START,
            move |input: StartInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let title = input.title.as_deref().map(str::trim).map(str::to_owned);
                    if title.as_deref() == Some("") {
                        return Err(ToolError::input(
                            "title: Too small: expected string to have >=1 characters",
                        ));
                    }
                    let data = deps
                        .run(
                            HostCommand::Start,
                            args(&[
                                ("project", Some(Value::String(input.project))),
                                ("prompt", Some(Value::String(input.prompt))),
                                ("idempotencyKey", Some(Value::String(input.idempotency_key))),
                                ("title", title.map(Value::String)),
                                ("model", input.model.map(Value::String)),
                                ("effort", input.effort.map(Value::String)),
                            ]),
                            LAUNCH,
                        )
                        .await?;
                    let session = to_session(&data);
                    let reused = data.get("reused") == Some(&Value::Bool(true));
                    if !reused {
                        deps.note_turn_started(&session, session.revision).await;
                    }
                    Ok::<_, ToolError>(
                        json!({ "session": session_json(&session), "reused": reused }),
                    )
                }
            },
        )?
    };

    let send = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSION_SEND,
            move |input: SendInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let data = deps
                        .run(
                            HostCommand::Send,
                            args(&[
                                ("session", Some(Value::String(input.session))),
                                ("prompt", Some(Value::String(input.prompt))),
                                ("interrupt", Some(Value::Bool(input.interrupt))),
                                ("idempotencyKey", input.idempotency_key.map(Value::String)),
                            ]),
                            LAUNCH,
                        )
                        .await?;
                    let session = to_session(&data);
                    let previous_revision = int_of(data.get("previous_revision"));
                    let reused = data.get("reused") == Some(&Value::Bool(true));
                    if !reused {
                        deps.note_turn_started(&session, previous_revision).await;
                    }
                    Ok::<_, ToolError>(json!({
                        "session": session_json(&session),
                        "previousRevision": previous_revision,
                        "reused": reused,
                        "warning": str_of(data.get("warning")),
                    }))
                }
            },
        )?
    };

    let stop = {
        let deps = deps.clone();
        typed_tool(
            &defs::CLAUDE_SESSION_STOP,
            move |input: StopInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let data = deps
                        .run(
                            HostCommand::Stop,
                            args(&[("session", Some(Value::String(input.session)))]),
                            QUICK,
                        )
                        .await?;
                    Ok::<_, ToolError>(json!({
                        "id": str_of(data.get("id")),
                        "sessionId": str_of(data.get("session_id")).unwrap_or_default(),
                    }))
                }
            },
        )?
    };

    Ok(vec![link_status, list, get, read, start, send, stop])
}
