//! System tools: capabilities, tasks and runs, and livestreams. Livestream
//! data comes from the owning package through `LiveDirectory` and
//! `LiveIntelligence`; none of these tools polls a platform.

pub mod defs;

use std::sync::Arc;

use omni_api::runs::Run;
use omni_api::streamers::LivestreamDetails;
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolMetaError, paginate, typed_tool};
use omni_runtime::ports::{PortError, Ports};
use omni_tasks::persistence::TaskRunData;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{TaskControl, conform, truncate};

/// Configuration facts `system_status` reports (never the values themselves).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConfiguredFeatures {
    pub icloud: bool,
    pub web_search: bool,
    /// The iOS live-control service is always constructed in production.
    pub ios_controls: bool,
    pub printing: bool,
}

fn set(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

impl ConfiguredFeatures {
    pub fn from_config(config: &Config) -> Self {
        Self {
            icloud: set(config.icloud_username.as_ref())
                && set(config.icloud_app_password.as_ref()),
            web_search: set(config.tavily_api_key.as_ref()),
            ios_controls: true,
            printing: set(config.printer_ipp_url.as_ref()),
        }
    }
}

/// What the system tools read.
#[derive(Clone)]
pub struct SystemDeps {
    pub tasks: Arc<dyn TaskControl>,
    pub ports: Ports,
    pub features: ConfiguredFeatures,
    pub clock: SharedClock,
}

fn port_error(error: PortError) -> ToolError {
    let retryable = matches!(
        error,
        PortError::Failed {
            transient: true,
            ..
        }
    );
    let unavailable = matches!(error, PortError::Unavailable(_));
    let tool = ToolError::execute(error.to_string()).retryable(retryable);
    if unavailable {
        tool.with_code("unavailable")
    } else {
        tool
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|e| ToolError::execute(e.to_string()))
}

/// Optional fields omitted rather than `null`.
fn run_json(run: &TaskRunData) -> Value {
    let mut out = Map::new();
    out.insert("runId".into(), json!(run.run_id));
    out.insert("taskName".into(), json!(run.task_name));
    out.insert("trigger".into(), json!(run.trigger));
    if let Some(at) = run.scheduled_for {
        out.insert("scheduledFor".into(), json!(at));
    }
    out.insert("startedAt".into(), json!(run.started_at));
    if let Some(at) = run.finished_at {
        out.insert("finishedAt".into(), json!(at));
    }
    out.insert("status".into(), json!(run.status));
    if let Some(error) = &run.error {
        out.insert("error".into(), json!(error));
    }
    if let Some(summary) = &run.summary {
        out.insert("summary".into(), json!(summary));
    }
    Value::Object(out)
}

/// The API `Run` (explicit nulls) as `taskRunSchema` (omitted fields).
fn api_run_json(run: &Run) -> Value {
    let mut value = json!(run);
    if let Some(map) = value.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    value
}

#[derive(Deserialize)]
struct EmptyInput {}

/// Schema defaults, for callers that bypass the endpoint's default filling.
mod defaults {
    pub fn limit() -> usize {
        25
    }
    pub fn log_limit() -> usize {
        100
    }
    pub fn log_message_chars() -> usize {
        2_000
    }
    pub fn metrics_days() -> i64 {
        30
    }
    pub fn session_limit() -> usize {
        20
    }
}

#[derive(Deserialize)]
struct PageInput {
    #[serde(default)]
    cursor: usize,
    #[serde(default = "defaults::limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRunInput {
    task_name: String,
    #[serde(default)]
    input: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRunsInput {
    #[serde(default)]
    task_name: Option<String>,
    #[serde(default)]
    cursor: usize,
    #[serde(default = "defaults::limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskRunGetInput {
    run_id: String,
    #[serde(default)]
    log_cursor: usize,
    #[serde(default = "defaults::log_limit")]
    log_limit: usize,
    #[serde(default = "defaults::log_message_chars")]
    max_message_chars: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LivestreamsInput {
    #[serde(default)]
    live_only: bool,
    #[serde(default)]
    cursor: usize,
    #[serde(default = "defaults::limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LivestreamGetInput {
    streamer_id: String,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default = "defaults::metrics_days")]
    metrics_days: i64,
    #[serde(default = "defaults::session_limit")]
    session_limit: usize,
    #[serde(default = "defaults::limit")]
    intelligence_event_limit: usize,
}

/// Trims an already length-checked string, which must stay non-empty.
fn trimmed(value: &str, field: &str) -> Result<String, ToolError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ToolError::input(format!(
            "{field}: Too small: expected string to have >=1 characters"
        )));
    }
    Ok(trimmed.to_owned())
}

/// The MCP livestream shape from a `LivestreamSummary` value.
fn livestream_json(summary: Value) -> Result<Value, ToolError> {
    conform(&defs::LIVESTREAMS_LIST, json!({ "livestreams": [summary] }))?
        .get_mut("livestreams")
        .and_then(Value::as_array_mut)
        .and_then(|items| items.pop())
        .ok_or_else(|| ToolError::execute("invalid livestream summary"))
}

pub fn system_tools(deps: &SystemDeps) -> Result<Vec<McpTool>, ToolMetaError> {
    let status = {
        let deps = deps.clone();
        typed_tool(
            &defs::SYSTEM_STATUS,
            move |_: EmptyInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let names: Vec<String> = deps
                        .tasks
                        .list()
                        .await
                        .map_err(ToolError::execute)?
                        .into_iter()
                        .map(|task| task.name)
                        .collect();
                    let livestreams = match deps.ports.live_directory() {
                        Some(directory) => {
                            !directory.streamers().await.map_err(port_error)?.is_empty()
                        }
                        None => false,
                    };
                    let features = deps.features;
                    Ok::<_, ToolError>(json!({
                        "capabilities": {
                            "taskControls": !names.is_empty(),
                            "livestreams": livestreams,
                            "livestreamIntelligence": deps.ports.live_intelligence().is_some(),
                            "iCloudEmail": deps.ports.email_reader().is_some() && features.icloud,
                            "iCloudCalendar": features.icloud,
                            "webSearch": features.web_search,
                            "iosControls": features.ios_controls,
                            "printing": features.printing,
                        }
                    }))
                }
            },
        )?
    };

    let tasks_list = {
        let deps = deps.clone();
        typed_tool(
            &defs::TASKS_LIST,
            move |input: PageInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let tasks = deps.tasks.list().await.map_err(ToolError::execute)?;
                    let page = paginate(tasks, input.cursor, input.limit);
                    let items: Vec<Value> = page
                        .items
                        .iter()
                        .map(|task| {
                            json!({
                                "name": task.name,
                                "displayName": task.display_name,
                                "schedule": task.schedule,
                                "running": task.running,
                                "nextRuns": task.next_runs,
                                "lastRun": task.last_run.as_ref().map(api_run_json),
                            })
                        })
                        .collect();
                    Ok::<_, ToolError>(json!({
                        "tasks": items,
                        "nextCursor": page.next_cursor,
                        "total": page.total,
                    }))
                }
            },
        )?
    };

    let task_run = {
        let deps = deps.clone();
        typed_tool(
            &defs::TASK_RUN,
            move |input: TaskRunInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let name = trimmed(&input.task_name, "taskName")?;
                    let run_id = deps
                        .tasks
                        .run_now(&name, input.input.map(Value::Object))
                        .map_err(ToolError::execute)?;
                    Ok::<_, ToolError>(json!({ "runId": run_id, "taskName": name, "queued": true }))
                }
            },
        )?
    };

    let task_runs_list = {
        let deps = deps.clone();
        typed_tool(
            &defs::TASK_RUNS_LIST,
            move |input: TaskRunsInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let name = input
                        .task_name
                        .as_deref()
                        .map(|name| trimmed(name, "taskName"))
                        .transpose()?;
                    let mut runs = deps
                        .tasks
                        .recent_runs(name.as_deref(), 501)
                        .await
                        .map_err(ToolError::execute)?;
                    let truncated = runs.len() > 500;
                    runs.truncate(500);
                    let page = paginate(runs, input.cursor, input.limit);
                    Ok::<_, ToolError>(json!({
                        "runs": page.items.iter().map(run_json).collect::<Vec<_>>(),
                        "nextCursor": page.next_cursor,
                        "total": page.total,
                        "resultWindowTruncated": truncated,
                    }))
                }
            },
        )?
    };

    let task_run_get = {
        let deps = deps.clone();
        typed_tool(
            &defs::TASK_RUN_GET,
            move |input: TaskRunGetInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let run_id = trimmed(&input.run_id, "runId")?;
                    let Some((run, lines, dropped)) = deps
                        .tasks
                        .run_logs(&run_id)
                        .await
                        .map_err(ToolError::execute)?
                    else {
                        return Err(ToolError::execute(format!("Unknown task run \"{run_id}\"")));
                    };
                    let page = paginate(lines, input.log_cursor, input.log_limit);
                    let logs: Vec<Value> = page
                        .items
                        .iter()
                        .map(|line| {
                            let (message, cut) = truncate(&line.msg, input.max_message_chars);
                            json!({
                                "timestamp": line.t,
                                "level": line.level,
                                "logger": line.logger,
                                "message": message,
                                "messageTruncated": cut,
                            })
                        })
                        .collect();
                    Ok::<_, ToolError>(json!({
                        "run": run_json(&run),
                        "logs": logs,
                        "logNextCursor": page.next_cursor,
                        "logTotal": page.total,
                        "droppedLogs": dropped,
                    }))
                }
            },
        )?
    };

    let livestreams_list = {
        let deps = deps.clone();
        typed_tool(
            &defs::LIVESTREAMS_LIST,
            move |input: LivestreamsInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let summaries = match deps.ports.live_directory() {
                        Some(directory) => directory.streamers().await.map_err(port_error)?,
                        None => Vec::new(),
                    };
                    let mut values = summaries
                        .into_iter()
                        .map(livestream_json)
                        .collect::<Result<Vec<_>, _>>()?;
                    values.retain(|v| !input.live_only || v["live"] == json!(true));
                    let viewers = |v: &Value| v["viewerCount"].as_f64().unwrap_or(0.0);
                    values.sort_by(|a, b| {
                        let live = |v: &Value| u8::from(v["live"] == json!(true));
                        live(b)
                            .cmp(&live(a))
                            .then_with(|| viewers(b).total_cmp(&viewers(a)))
                            .then_with(|| {
                                omni_core::js::locale_compare(
                                    a["displayName"].as_str().unwrap_or(""),
                                    b["displayName"].as_str().unwrap_or(""),
                                )
                            })
                    });
                    let page = paginate(values, input.cursor, input.limit);
                    Ok::<_, ToolError>(json!({
                        "livestreams": page.items,
                        "nextCursor": page.next_cursor,
                        "total": page.total,
                    }))
                }
            },
        )?
    };

    let livestream_get = {
        let deps = deps.clone();
        typed_tool(
            &defs::LIVESTREAM_GET,
            move |input: LivestreamGetInput, _: ToolContext| {
                let deps = deps.clone();
                async move {
                    let streamer_id = trimmed(&input.streamer_id, "streamerId")?;
                    let unknown =
                        || ToolError::execute(format!("Unknown livestream \"{streamer_id}\""));
                    let directory = deps.ports.live_directory().ok_or_else(unknown)?;
                    let details: LivestreamDetails = decode(
                        directory
                            .details(&streamer_id)
                            .await
                            .map_err(port_error)?
                            .ok_or_else(unknown)?,
                    )?;
                    let wants = |part: &str| input.include.iter().any(|i| i == part);
                    let cutoff = deps.clock.now_ms() - input.metrics_days * 86_400_000;
                    let metrics = wants("metrics").then(|| {
                        let mut metrics = details.metrics.clone();
                        metrics.daily_buckets.retain(|b| b.timestamp >= cutoff);
                        for platform in &mut metrics.platforms {
                            platform.daily_buckets.retain(|b| b.timestamp >= cutoff);
                        }
                        metrics
                    });
                    let sessions = wants("sessions").then(|| {
                        let mut sessions = details.sessions.clone();
                        sessions.sort_by_key(|s| std::cmp::Reverse(s.ended_at));
                        sessions.truncate(input.session_limit);
                        sessions
                    });
                    let intelligence = if wants("intelligence") {
                        Some(match deps.ports.live_intelligence() {
                            Some(port) => {
                                let found = port
                                    .details(&streamer_id, input.intelligence_event_limit)
                                    .await
                                    .map_err(port_error)?
                                    .unwrap_or(Value::Null);
                                json!({
                                    "current": found.get("intelligence").cloned().unwrap_or(Value::Null),
                                    "diagnostics": found.get("diagnostics").cloned().unwrap_or(Value::Null),
                                    "events": found.get("events").cloned().unwrap_or_else(|| json!([])),
                                    "runtime": found.get("runtime").cloned().unwrap_or(Value::Null),
                                })
                            }
                            None => json!({
                                "current": null,
                                "diagnostics": null,
                                "events": [],
                                "runtime": null,
                            }),
                        })
                    } else {
                        None
                    };
                    let livestream = livestream_json(
                        serde_json::to_value(&details.livestream)
                            .map_err(|e| ToolError::execute(e.to_string()))?,
                    )?;
                    conform(
                        &defs::LIVESTREAM_GET,
                        json!({
                            "livestream": livestream,
                            "metrics": metrics,
                            "sessions": sessions,
                            "intelligence": intelligence,
                        }),
                    )
                }
            },
        )?
    };

    Ok(vec![
        status,
        tasks_list,
        task_run,
        task_runs_list,
        task_run_get,
        livestreams_list,
        livestream_get,
    ])
}
