//! Shared fixtures for the omni-mcp integration tests.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use futures::future::BoxFuture;
use omni_api::tasks::TaskInfo;
use omni_core::clock::TestClock;
use omni_mcp::activity::ActivityRecorder;
use omni_mcp::endpoint;
use omni_mcp::events::service::McpEventService;
use omni_mcp::rpc::McpProtocol;
use omni_mcp::tools::TOOL_ORDER;
use omni_mcp::tools::{RunWithLogs, TaskControl};
use omni_mcp_kit::{
    McpTool, ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMeta, ToolOutput, raw_tool,
};
use omni_store::Store;
use omni_tasks::persistence::TaskRunData;
use omni_testkit::TestStore;
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;
use tower::ServiceExt;

pub const TOKEN: &str = "test-token-0123456789-ABCDEFGHIJKLMNOPQRSTUVWXYZ";
pub const EPOCH_MS: i64 = 1_790_000_000_000;

pub fn clock() -> Arc<TestClock> {
    TestClock::new(EPOCH_MS)
}

pub async fn test_store(clock: &Arc<TestClock>) -> TestStore {
    TestStore::new(clock.clone()).await
}

struct Unwired(String);

impl ToolHandler for Unwired {
    fn call<'a>(
        &'a self,
        _input: Value,
        _cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let message = format!(
            "{} is owned by another package and not wired in this test",
            self.0
        );
        Box::pin(async move { Err(ToolError::execute(message)) })
    }
}

/// Every package's tool definitions, in serving order ([`TOOL_ORDER`]).
pub fn all_defs() -> Vec<&'static dyn ToolDefinition> {
    let packages: [&[&'static dyn ToolDefinition]; 17] = [
        &omni_reminders::mcp::defs::TOOLS,
        &omni_mcp::tools::system::defs::TOOLS,
        &omni_email::mcp_tools::defs::TOOLS,
        &omni_calendar::mcp::defs::TOOLS,
        &omni_imap::mcp_tools::defs::TOOLS,
        &omni_live::mcp::defs::TOOLS,
        &omni_mcp::tools::events::defs::TOOLS,
        &omni_media::mcp::defs::TOOLS,
        &omni_podcasts::mcp::defs::TOOLS,
        &omni_presspods::mcp::defs::TOOLS,
        &omni_parcel::mcp::defs::TOOLS,
        &omni_personal::mcp::personal::defs::TOOLS,
        &omni_personal::mcp::printer::defs::TOOLS,
        &omni_personal::mcp::browser_history::defs::TOOLS,
        &omni_personal::mcp::owner_notify::defs::TOOLS,
        &omni_stylist::mcp::defs::TOOLS,
        &omni_mcp::tools::claude_sessions::defs::TOOLS,
    ];
    let mut by_name: HashMap<&str, &'static dyn ToolDefinition> = HashMap::new();
    for def in packages.into_iter().flatten() {
        assert!(
            by_name.insert(def.name(), *def).is_none(),
            "duplicate tool {}",
            def.name()
        );
    }
    let ordered: Vec<&'static dyn ToolDefinition> = TOOL_ORDER
        .iter()
        .map(|name| {
            by_name
                .remove(name)
                .unwrap_or_else(|| panic!("no definition for {name}"))
        })
        .collect();
    assert!(
        by_name.is_empty(),
        "tools missing from TOOL_ORDER: {:?}",
        by_name.keys()
    );
    ordered
}

/// The definition of `name`.
pub fn def(name: &str) -> &'static dyn ToolDefinition {
    all_defs()
        .into_iter()
        .find(|def| def.name() == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
}

/// The metadata of `name`.
pub fn meta(name: &str) -> &'static ToolMeta {
    def(name).meta().unwrap()
}

/// Every tool not in `own`, with a handler that fails.
pub fn other_tools(own: &[McpTool]) -> Vec<McpTool> {
    let names: HashSet<&str> = own.iter().map(|tool| tool.meta.name.as_str()).collect();
    all_defs()
        .into_iter()
        .filter(|def| !names.contains(def.name()))
        .map(|def| raw_tool(def, Arc::new(Unwired(def.name().to_owned()))).unwrap())
        .collect()
}

/// A tool backed by a closure (for tools owned by other packages).
pub struct FnTool<F>(pub F);

impl<F> ToolHandler for FnTool<F>
where
    F: Fn(Value) -> Result<ToolOutput, ToolError> + Send + Sync,
{
    fn call<'a>(
        &'a self,
        input: Value,
        _cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        let result = (self.0)(input);
        Box::pin(async move { result })
    }
}

/// A fake task registry.
#[derive(Default)]
pub struct FakeTasks {
    pub tasks: Vec<TaskInfo>,
    pub runs: Vec<TaskRunData>,
    /// The run id `run_now` returns (`mock-run-123` when unset).
    pub run_id: Option<&'static str>,
    pub run_now_calls: Mutex<Vec<(String, Option<Value>)>>,
}

impl TaskControl for FakeTasks {
    fn list(&self) -> BoxFuture<'_, Result<Vec<TaskInfo>, String>> {
        let tasks = self.tasks.clone();
        Box::pin(async move { Ok(tasks) })
    }

    fn run_now(&self, name: &str, input: Option<Value>) -> Result<String, String> {
        self.run_now_calls
            .lock()
            .unwrap()
            .push((name.to_owned(), input));
        Ok(self.run_id.unwrap_or("mock-run-123").to_owned())
    }

    fn recent_runs<'a>(
        &'a self,
        task: Option<&'a str>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TaskRunData>, String>> {
        let runs: Vec<TaskRunData> = self
            .runs
            .iter()
            .filter(|run| task.is_none_or(|t| run.task_name == t))
            .take(limit)
            .cloned()
            .collect();
        Box::pin(async move { Ok(runs) })
    }

    fn run_logs<'a>(
        &'a self,
        run_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<RunWithLogs>, String>> {
        let run = self.runs.iter().find(|run| run.run_id == run_id).cloned();
        Box::pin(async move { Ok(run.map(|run| (run, Vec::new(), 0))) })
    }
}

/// An MCP router serving `own` plus failing placeholders for every other tool.
pub fn mcp_router(
    store: &Store,
    clock: &Arc<TestClock>,
    own: Vec<McpTool>,
    events: Option<McpEventService>,
) -> Router {
    let mut tools = other_tools(&own);
    tools.extend(own);
    let recorder = ActivityRecorder::new(store.clone(), clock.clone(), TaskTracker::new());
    let protocol = McpProtocol::new(tools, recorder, events).unwrap();
    endpoint::router(Some(TOKEN), Some(protocol))
}

/// One raw HTTP exchange.
pub struct Exchange {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Exchange {
    /// JSON-RPC messages from a JSON or SSE body.
    pub fn messages(&self) -> Vec<Value> {
        let trimmed = self.text.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return match serde_json::from_str::<Value>(trimmed).unwrap() {
                Value::Array(items) => items,
                other => vec![other],
            };
        }
        trimmed
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| serde_json::from_str(data.trim()).unwrap())
            .collect()
    }

    pub fn message(&self) -> Value {
        self.messages().into_iter().next().unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub async fn send(router: &Router, request: Request<Body>) -> Exchange {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Exchange {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// A legacy-era JSON-RPC POST with the MCP token.
pub fn legacy(body: &Value) -> Request<Body> {
    request(body, &[("mcp-protocol-version", "2025-11-25")])
}

/// A POST with the MCP token and extra headers.
pub fn request(body: &Value, extra: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// `tools/call` in the legacy era; returns the JSON-RPC message.
pub async fn call_tool(router: &Router, name: &str, arguments: Value) -> Value {
    let exchange = send(
        router,
        legacy(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        })),
    )
    .await;
    exchange.message()
}

/// The `result` of a tools/call message.
pub fn result(message: &Value) -> &Value {
    &message["result"]
}

pub fn is_error(message: &Value) -> bool {
    message["result"]["isError"] == json!(true)
}

pub fn error_text(message: &Value) -> String {
    message["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}
