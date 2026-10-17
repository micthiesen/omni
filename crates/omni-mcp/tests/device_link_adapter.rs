//! End to end: Claude session tools over the real device-link relay.
//!
//! `DeviceLinkService` is the `ClaudeHost` port implementation. A simulated
//! host long-polls the relay and answers with canned session-client envelopes; results must reach MCP with the host name and
//! home directory scrubbed, and errors keep their codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use omni_device_link::{DeviceCommand, DeviceJobOutcome, DeviceLinkService, PollReport};
use omni_mcp::tools::claude_sessions::{ClaudeDeps, claude_session_tools};
use serde_json::json;

/// Answers every job the way the host's session client would.
fn spawn_host(link: DeviceLinkService) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let jobs = link
                .poll(PollReport {
                    disabled: false,
                    host: Some("MaxBook".to_owned()),
                })
                .await;
            for job in jobs {
                let outcome = match job.command {
                    DeviceCommand::Projects => DeviceJobOutcome::Output(json!({
                        "v": 1, "ok": true,
                        "data": {"projects": [{"name": "omni", "path": "/Users/michael/Code/omni", "exists": true}]}
                    })),
                    DeviceCommand::List => DeviceJobOutcome::Output(json!({
                        "v": 1, "ok": true,
                        "data": {"sessions": [{
                            "session_id": "abc-123", "status": "idle", "revision": 2,
                            "cwd": "/Users/michael/Code/omni",
                            "last_assistant": "Built it on MaxBook."
                        }]}
                    })),
                    _ => DeviceJobOutcome::Output(json!({
                        "v": 1, "ok": false,
                        "error": {"code": "busy", "message": "MaxBook session /Users/michael/x is mid-turn"}
                    })),
                };
                link.complete(&job.id, outcome);
            }
        }
    })
}

#[tokio::test]
async fn claude_tools_reach_the_host_only_through_the_long_poll_and_never_name_it() {
    let clock = clock();
    let db = test_store(&clock).await;
    let link = DeviceLinkService::new(clock.clone());
    let host = spawn_host(link.clone());
    let tools = claude_session_tools(&ClaudeDeps {
        host: Some(Arc::new(link.clone())),
        watcher: None,
    })
    .unwrap();
    let router = mcp_router(&db.store, &clock, tools, None);
    // Let the simulated host's first poll register.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let status = call_tool(&router, "claude_link_status", json!({})).await;
    let body = &status["result"]["structuredContent"];
    assert_eq!(body["online"], true);
    assert_eq!(body["projects"][0]["path"], "~/Code/omni");
    assert!(body.get("host").is_none());

    let listed = call_tool(&router, "claude_sessions_list", json!({})).await;
    let session = &listed["result"]["structuredContent"]["sessions"][0];
    assert_eq!(session["cwd"], "~/Code/omni");
    assert_eq!(session["lastAssistant"], "Built it on the host.");

    let failed = call_tool(
        &router,
        "claude_session_send",
        json!({"session": "abc-123", "prompt": "go"}),
    )
    .await;
    assert!(is_error(&failed));
    assert_eq!(
        error_text(&failed),
        "the host session ~/x is mid-turn (busy)"
    );
    let text = format!("{status}{listed}{failed}");
    assert!(!text.contains("MaxBook"));
    assert!(!text.contains("/Users/"));
    host.abort();
}
