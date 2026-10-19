//! Claude session tools. A real watcher records each turn and the cases read
//! the `mcp-claude-session-watch` row it wrote.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{clock, test_store};
use futures::future::BoxFuture;
use omni_mcp::events::claude_sessions::{ClaudeSessionWatch, ClaudeSessionWatcher};
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::host::{ClaudeHost, HostCommand, HostError, HostLinkStatus};
use omni_mcp::tools::claude_sessions::{
    ClaudeDeps, claude_session_tools, scrub_host_details, to_item, to_session,
};
use omni_mcp_kit::{McpTool, ToolOutput, registry::standalone_context};
use omni_runtime::ports::Ports;
use omni_store::Store;
use omni_store::entity::EntityOps;
use serde_json::{Map, Value, json};

const SESSION: &str = "25ffb449-1111-4222-8333-444455556666";

type Calls = Arc<Mutex<Vec<(HostCommand, Map<String, Value>)>>>;

#[derive(Clone, Default)]
struct Host {
    responses: Arc<HashMap<&'static str, Value>>,
    calls: Calls,
}

impl ClaudeHost for Host {
    fn status(&self) -> HostLinkStatus {
        HostLinkStatus {
            online: true,
            disabled: false,
            host: Some("studio".to_owned()),
            last_seen_at: None,
            pending_jobs: 0,
        }
    }

    fn execute(
        &self,
        command: HostCommand,
        args: Map<String, Value>,
        _timeout: Duration,
    ) -> BoxFuture<'_, Result<Map<String, Value>, HostError>> {
        self.calls.lock().unwrap().push((command, args));
        let response = self
            .responses
            .get(command.as_str())
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(response) })
    }
}

struct Accept;

impl WebhookPort for Accept {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        _: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        Box::pin(async { Ok(204) })
    }
}

struct Harness {
    host: Host,
    tools: HashMap<String, McpTool>,
    names: Vec<String>,
    store: Store,
    _db: omni_testkit::TestStore,
}

impl Harness {
    async fn new(responses: Vec<(&'static str, Value)>) -> Self {
        let clock = clock();
        let db = test_store(&clock).await;
        let host = Host {
            responses: Arc::new(responses.into_iter().collect()),
            ..Host::default()
        };
        let events = McpEventService::new(
            "test-omni-bearer",
            db.store.clone(),
            clock.clone(),
            Arc::new(Accept),
            Ports::default(),
        );
        let watcher =
            ClaudeSessionWatcher::new(events, Arc::new(host.clone()), db.store.clone(), clock);
        let list = claude_session_tools(&ClaudeDeps {
            host: Some(Arc::new(host.clone())),
            watcher: Some(Arc::new(watcher)),
        })
        .unwrap();
        Self {
            host,
            names: list.iter().map(|t| t.meta.name.clone()).collect(),
            tools: list.into_iter().map(|t| (t.meta.name.clone(), t)).collect(),
            store: db.store.clone(),
            _db: db,
        }
    }

    async fn call(&self, name: &str, input: Value) -> Value {
        match self.tools[name]
            .handler
            .call(input, standalone_context("test"))
            .await
            .unwrap()
        {
            ToolOutput::Structured(map) => Value::Object(map),
            ToolOutput::Custom { structured, .. } => Value::Object(structured),
        }
    }

    fn commands(&self) -> Vec<HostCommand> {
        self.host
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| *c)
            .collect()
    }

    async fn noted(&self) -> Vec<ClaudeSessionWatch> {
        self.store
            .read(|docs| docs.get_all::<ClaudeSessionWatch>())
            .await
            .unwrap()
    }
}

fn summary(revision: i64, status: &str) -> Value {
    json!({
        "id": "25ffb449",
        "session_id": SESSION,
        "status": status,
        "state": if status == "busy" { "working" } else { "idle" },
        "revision": revision,
    })
}

fn with(base: Value, extra: Value) -> Value {
    let mut base = base;
    for (key, value) in extra.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::test(start_paused = true)]
async fn keeps_one_read_tool_for_status_waiting_and_results() {
    let harness = Harness::new(Vec::new()).await;
    assert_eq!(
        harness.names,
        [
            "claude_link_status",
            "claude_sessions_list",
            "claude_session_get",
            "claude_session_read",
            "claude_session_start",
            "claude_session_send",
            "claude_session_stop",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn waits_then_reads_the_result_only_once_the_turn_finished() {
    let finished = Harness::new(vec![
        (
            "wait",
            with(summary(6, "idle"), json!({"timed_out": false})),
        ),
        (
            "result",
            with(summary(6, "idle"), json!({"result": "Done."})),
        ),
    ])
    .await;
    let output = finished
        .call(
            "claude_session_get",
            json!({"session": "25ffb449", "afterRevision": 4, "waitSeconds": 30, "includeResult": true}),
        )
        .await;
    assert_eq!(
        finished.commands(),
        [HostCommand::Wait, HostCommand::Result]
    );
    assert_eq!(
        Value::Object(finished.host.calls.lock().unwrap()[0].1.clone()),
        json!({"session": "25ffb449", "after": 4, "timeout": 30})
    );
    assert_eq!(output["timedOut"], false);
    assert_eq!(output["result"], "Done.");

    let pending = Harness::new(vec![(
        "wait",
        with(summary(5, "busy"), json!({"timed_out": true})),
    )])
    .await;
    let waited = pending
        .call(
            "claude_session_get",
            json!({"session": "25ffb449", "waitSeconds": 10, "includeResult": true}),
        )
        .await;
    assert_eq!(pending.commands(), [HostCommand::Wait]);
    assert_eq!(waited["timedOut"], true);
    assert_eq!(waited["result"], Value::Null);

    let status = Harness::new(vec![("status", summary(5, "idle"))]).await;
    status
        .call("claude_session_get", json!({"session": "25ffb449"}))
        .await;
    assert_eq!(status.commands(), [HostCommand::Status]);
}

#[tokio::test(start_paused = true)]
async fn forwards_the_send_key_and_notes_only_new_turns_for_events() {
    let sent = Harness::new(vec![(
        "send",
        with(summary(5, "busy"), json!({"previous_revision": 4})),
    )])
    .await;
    let output = sent
        .call(
            "claude_session_send",
            json!({"session": "25ffb449", "prompt": "Continue", "idempotencyKey": "dot:1"}),
        )
        .await;
    assert_eq!(
        sent.host.calls.lock().unwrap()[0].1["idempotencyKey"],
        "dot:1"
    );
    assert_eq!(output["previousRevision"], 4);
    assert_eq!(output["reused"], false);
    let noted = sent.noted().await;
    assert_eq!(noted.len(), 1);
    assert_eq!(noted[0].session_id, SESSION);
    assert_eq!(noted[0].id.as_deref(), Some("25ffb449"));
    assert_eq!(noted[0].project, None);
    assert_eq!(noted[0].revision, 4.0);
    assert_eq!(noted[0].started, Some(true));

    let reused = Harness::new(vec![(
        "send",
        with(
            summary(6, "idle"),
            json!({"previous_revision": 4, "reused": true}),
        ),
    )])
    .await;
    reused
        .call(
            "claude_session_send",
            json!({"session": "25ffb449", "prompt": "Continue", "idempotencyKey": "dot:1"}),
        )
        .await;
    assert!(reused.noted().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn reports_projects_with_the_link_status() {
    let harness = Harness::new(vec![(
        "projects",
        json!({"projects": [{"name": "omni", "path": "~/Code/omni", "exists": true}]}),
    )])
    .await;
    let output = harness.call("claude_link_status", json!({})).await;
    assert_eq!(output["online"], true);
    assert_eq!(output["projects"][0]["name"], "omni");
    assert_eq!(output["projects"][0]["exists"], true);
    assert_eq!(output["projectsError"], Value::Null);
    assert!(output.get("host").is_none());
}

#[test]
fn maps_claude_for_dot_summaries_to_the_mcp_session_shape() {
    let session = to_session(&object(json!({
        "id": "a1b2c3",
        "session_id": "a1b2c3d4-0000-0000-0000-000000000000",
        "kind": "background",
        "title": "Fix reconnect",
        "cwd": "/Users/michael/Code/omni",
        "project": "omni",
        "status": "idle",
        "state": "done",
        "started_at": 1_790_000_000_000_i64,
        "revision": 12,
        "last_assistant": "Done.",
        "managed": true,
    })));
    assert_eq!(
        serde_json::to_value(session).unwrap(),
        json!({
            "id": "a1b2c3",
            "sessionId": "a1b2c3d4-0000-0000-0000-000000000000",
            "kind": "background",
            "title": "Fix reconnect",
            "cwd": "/Users/michael/Code/omni",
            "project": "omni",
            "status": "idle",
            "state": "done",
            "startedAt": omni_core::js::to_iso_string(1_790_000_000_000),
            "revision": 12,
            "lastAssistant": "Done.",
        })
    );
}

#[test]
fn tolerates_missing_fields_from_history_only_sessions() {
    let session = to_session(&object(json!({"session_id": "abc", "status": "stopped"})));
    assert_eq!(session.id, None);
    assert_eq!(session.project, None);
    assert_eq!(session.started_at, None);
    assert_eq!(session.revision, 0);
}

#[test]
fn bounds_transcript_text_and_tool_inputs() {
    let item = to_item(&object(json!({
        "index": 3,
        "kind": "assistant",
        "timestamp": "2026-10-04T00:00:00Z",
        "text": "x".repeat(9_000),
    })));
    assert_eq!(item.text.as_deref().map(str::len), Some(8_000));
    assert!(item.truncated);
    let tool = to_item(&object(json!({
        "index": 4,
        "kind": "tool_use",
        "tool": "Bash",
        "input": {"command": "y".repeat(2_000)},
    })));
    assert_eq!(tool.tool.as_deref(), Some("Bash"));
    assert_eq!(tool.text, None);
    assert!(!tool.truncated);
    assert_eq!(tool.input.as_deref().map(str::len), Some(1_000));
}

#[test]
fn removes_the_host_name_and_home_directory_from_results() {
    assert_eq!(
        scrub_host_details(
            &json!({
                "cwd": "/Users/michael/Code/omni",
                "items": [{"text": "Ran on MaxBook in /Users/michael/.dotfiles"}],
                "revision": 3,
            }),
            Some("MaxBook"),
        ),
        json!({
            "cwd": "~/Code/omni",
            "items": [{"text": "Ran on the host in ~/.dotfiles"}],
            "revision": 3,
        })
    );
    assert_eq!(
        scrub_host_details(&json!("maxbookish"), Some("MaxBook")),
        json!("maxbookish")
    );
    assert_eq!(
        scrub_host_details(&json!("MAXBOOK.local says hi"), Some("MaxBook")),
        json!("the host.local says hi")
    );
}
