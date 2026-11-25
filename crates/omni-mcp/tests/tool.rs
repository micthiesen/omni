//! The tool contract through the endpoint, which applies it: input validation
//! against the derived schema and defaults before the handler, defects turned
//! into failed results, output projected onto the output schema.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use common::*;
use omni_mcp_kit::{ToolError, ToolOutput, raw_tool, typed_tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[tokio::test]
async fn executes_and_preserves_structured_output() {
    let clock = clock();
    let store = test_store(&clock).await;
    #[derive(Deserialize)]
    struct In {
        #[serde(rename = "taskName")]
        task_name: String,
    }
    #[derive(Serialize)]
    struct Out {
        #[serde(rename = "runId")]
        run_id: String,
        #[serde(rename = "taskName")]
        task_name: String,
        queued: bool,
    }
    let tool = typed_tool(
        &omni_mcp::tools::system::defs::TASK_RUN,
        |input: In, _| async move {
            Ok::<_, ToolError>(Out {
                run_id: "r".into(),
                task_name: input.task_name.to_uppercase(),
                queued: true,
            })
        },
    )
    .unwrap();
    let router = mcp_router(&store.store, &clock, vec![tool], None);
    let message = call_tool(&router, "task_run", json!({"taskName": "ok"})).await;
    assert_eq!(
        result(&message)["structuredContent"],
        json!({"runId": "r", "taskName": "OK", "queued": true})
    );
}

#[tokio::test]
async fn turns_thrown_defects_into_a_safe_tagged_tool_failure() {
    let clock = clock();
    let store = test_store(&clock).await;
    let tool = raw_tool(
        def("system_status"),
        Arc::new(FnTool(|_| -> Result<ToolOutput, ToolError> {
            panic!("private implementation failure")
        })),
    )
    .unwrap();
    let router = mcp_router(&store.store, &clock, vec![tool], None);
    let message = call_tool(&router, "system_status", json!({})).await;
    assert!(is_error(&message));
    assert_eq!(
        error_text(&message),
        "[failed] private implementation failure"
    );
}

#[tokio::test]
async fn enforces_declared_input_defaults_and_output_validation() {
    let clock = clock();
    let store = test_store(&clock).await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let record = seen.clone();
    #[derive(Deserialize)]
    struct In {
        cursor: i64,
        limit: i64,
    }
    let tool = typed_tool(
        &omni_mcp::tools::system::defs::TASKS_LIST,
        move |input: In, _| {
            record
                .lock()
                .unwrap()
                .push(json!([input.cursor, input.limit]));
            async move {
                // Violates the output schema (`tasks` must be an array).
                Ok::<_, ToolError>(json!({"tasks": "nope", "nextCursor": null, "total": 0}))
            }
        },
    )
    .unwrap();
    let router = mcp_router(&store.store, &clock, vec![tool], None);
    let defaulted = call_tool(&router, "tasks_list", json!({})).await;
    assert!(is_error(&defaulted));
    assert_eq!(*seen.lock().unwrap(), vec![json!([0, 25])]);
    let invalid = call_tool(
        &router,
        "tasks_list",
        json!({"cursor": 0, "unexpected": true}),
    )
    .await;
    assert!(is_error(&invalid));
    assert!(error_text(&invalid).starts_with("Input validation error"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}
