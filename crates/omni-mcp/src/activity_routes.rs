//! Read-only observability routes for MCP calls and the Claude Code host's
//! sessions. Omni's own UI may show the host
//! name, so these responses are not scrubbed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use omni_api::claude::{
    ClaudeActivityResponse, ClaudeLinkView, ClaudeProject, ClaudeProjectsResponse,
    ClaudeSessionsResponse, ClaudeTranscriptResponse,
};
use omni_api::mcp_activity::McpCallStatus;
use omni_core::clock::SharedClock;
use omni_server_kit::ApiError;
use omni_store::Store;
use serde_json::{Map, Value, json};

use crate::activity::{ActivityQuery, get_mcp_activity};
use crate::host::{ClaudeHost, HostCommand, HostError};
use crate::json::json_response;
use crate::tools::claude_sessions::{to_item, to_session};

const LIVE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct RoutesState {
    store: Store,
    clock: SharedClock,
    host: Option<Arc<dyn ClaudeHost>>,
}

type Params = Query<HashMap<String, String>>;

/// A positive integer `Number(value)`, capped, else the fallback.
fn bounded_int(value: Option<&String>, fallback: usize, max: usize) -> usize {
    let parsed = value.map_or(f64::NAN, |v| omni_core::js::string_to_number(v));
    if parsed.is_finite() && parsed.fract() == 0.0 && parsed > 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (parsed.min(max as f64) as usize)
    } else {
        fallback
    }
}

/// `^[0-9A-Za-z-]{4,64}$`.
fn valid_session(session: &str) -> bool {
    (4..=64).contains(&session.len())
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn link_failure(error: &HostError) -> Response {
    let unavailable = ["offline", "disabled", "not_picked_up", "not_configured"];
    let status = if unavailable.contains(&error.code.as_str()) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    };
    json_response(status, &json!({"error": error.detail, "code": error.code}))
}

fn ok<T: serde::Serialize>(body: &T) -> Response {
    json_response(
        StatusCode::OK,
        &serde_json::to_value(body).unwrap_or(Value::Null),
    )
}

impl RoutesState {
    async fn live(
        &self,
        command: HostCommand,
        args: Map<String, Value>,
    ) -> Result<Map<String, Value>, Response> {
        let Some(host) = &self.host else {
            return Err(link_failure(&HostError::new(
                "not_configured",
                "The Claude Code host link is not configured",
                false,
            )));
        };
        host.execute(command, args, LIVE_TIMEOUT)
            .await
            .map_err(|error| link_failure(&error))
    }
}

pub fn router(store: Store, clock: SharedClock, host: Option<Arc<dyn ClaudeHost>>) -> Router {
    Router::new()
        .route("/api/mcp/activity", get(mcp_activity))
        .route("/api/claude/activity", get(claude_activity))
        .route("/api/claude/sessions", get(claude_sessions))
        .route(
            "/api/claude/sessions/{session}/transcript",
            get(claude_transcript),
        )
        .route("/api/claude/projects", get(claude_projects))
        .with_state(RoutesState { store, clock, host })
}

async fn mcp_activity(
    State(state): State<RoutesState>,
    Query(params): Params,
) -> Result<Response, ApiError> {
    let before = params
        .get("before")
        .map_or(f64::NAN, |v| omni_core::js::string_to_number(v));
    let query = ActivityQuery {
        limit: bounded_int(params.get("limit"), 100, 200),
        tool: params.get("tool").filter(|t| !t.is_empty()).cloned(),
        status: params.get("status").and_then(|s| McpCallStatus::parse(s)),
        #[allow(clippy::cast_possible_truncation)]
        before: (before.is_finite() && before > 0.0).then_some(before as i64),
        tool_prefix: None,
    };
    let activity = get_mcp_activity(&state.store, &query, state.clock.now_ms())
        .await
        .map_err(ApiError::internal)?;
    Ok(ok(&activity))
}

async fn claude_activity(
    State(state): State<RoutesState>,
    Query(params): Params,
) -> Result<Response, ApiError> {
    let query = ActivityQuery {
        limit: bounded_int(params.get("limit"), 200, 500),
        tool_prefix: Some("claude_".to_owned()),
        ..ActivityQuery::default()
    };
    let activity = get_mcp_activity(&state.store, &query, state.clock.now_ms())
        .await
        .map_err(ApiError::internal)?;
    let link = match &state.host {
        Some(host) => {
            let status = host.status();
            ClaudeLinkView {
                configured: true,
                online: status.online,
                disabled: status.disabled,
                host: status.host,
                last_seen_at: status.last_seen_at,
                pending_jobs: status.pending_jobs as u64,
            }
        }
        None => ClaudeLinkView {
            configured: false,
            online: false,
            disabled: false,
            host: None,
            last_seen_at: None,
            pending_jobs: 0,
        },
    };
    Ok(ok(&ClaudeActivityResponse {
        link,
        actions: activity.calls,
        retention: activity.retention,
    }))
}

async fn claude_sessions(State(state): State<RoutesState>, Query(params): Params) -> Response {
    let mut args = Map::new();
    args.insert(
        "all".into(),
        Value::Bool(params.get("includeStopped").map(String::as_str) == Some("true")),
    );
    args.insert(
        "limit".into(),
        json!(bounded_int(params.get("limit"), 25, 100)),
    );
    match state.live(HostCommand::List, args).await {
        Ok(data) => ok(&ClaudeSessionsResponse {
            sessions: records(data.get("sessions"))
                .iter()
                .map(to_session)
                .collect(),
        }),
        Err(response) => response,
    }
}

async fn claude_transcript(
    State(state): State<RoutesState>,
    Path(session): Path<String>,
    Query(params): Params,
) -> Response {
    if !valid_session(&session) {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "Invalid session id", "code": "bad_request"}),
        );
    }
    let cursor = params
        .get("cursor")
        .map_or(f64::NAN, |v| omni_core::js::string_to_number(v));
    let mut args = Map::new();
    args.insert("session".into(), Value::String(session.clone()));
    args.insert(
        "limit".into(),
        json!(bounded_int(params.get("limit"), 40, 100)),
    );
    if cursor.is_finite() && cursor.fract() == 0.0 && cursor >= 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        args.insert("cursor".into(), json!(cursor as i64));
    }
    match state.live(HostCommand::Read, args).await {
        Ok(data) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let revision = data
                .get("revision")
                .and_then(Value::as_f64)
                .map_or(0, |n| n as u64);
            ok(&ClaudeTranscriptResponse {
                session_id: data
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map_or(session, str::to_owned),
                items: records(data.get("items")).iter().map(to_item).collect(),
                revision,
                #[allow(clippy::cast_possible_truncation)]
                next_cursor: data
                    .get("next_cursor")
                    .and_then(Value::as_f64)
                    .map(|n| n as i64),
                has_more: data.get("has_more") == Some(&Value::Bool(true)),
            })
        }
        Err(response) => response,
    }
}

async fn claude_projects(State(state): State<RoutesState>) -> Response {
    match state.live(HostCommand::Projects, Map::new()).await {
        Ok(data) => ok(&ClaudeProjectsResponse {
            projects: records(data.get("projects"))
                .iter()
                .map(|project| ClaudeProject {
                    name: display(project.get("name")),
                    path: display(project.get("path")),
                    exists: project.get("exists") == Some(&Value::Bool(true)),
                })
                .collect(),
        }),
        Err(response) => response,
    }
}

/// The routes' own `records()`: array items that are objects (arrays count,
/// reading as empty records); `null` and primitives are dropped.
fn records(value: Option<&Value>) -> Vec<Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    Value::Object(map) => Some(map.clone()),
                    Value::Array(_) => Some(Map::new()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// JS `String(value)` of a JSON value.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n
            .as_f64()
            .map_or_else(|| n.to_string(), omni_core::js::number_to_string),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

/// `String(value ?? "")`.
fn display(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(other) => js_string(other),
    }
}
