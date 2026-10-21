//! The `shell` tool: read-only tools as commands and `/omni` files, sessions,
//! output bounds, and the `shell_usage` measurements.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use common::*;
use omni_core::clock::TestClock;
use omni_mcp::activity::McpCallData;
use omni_mcp::tools::shell::{ShellDeps, ShellOutcome, ShellRunData, shell_tools};
use omni_mcp_kit::{McpTool, ToolError, ToolOutput, raw_tool};
use omni_store::Store;
use omni_store::entity::EntityOps;
use omni_testkit::TestStore;
use serde_json::{Value, json};

type Inputs = Arc<Mutex<Vec<(String, Value)>>>;

/// A read-only tool answering `output`, recording each input.
fn fake(name: &str, inputs: &Inputs, output: Value) -> McpTool {
    let inputs = inputs.clone();
    let tool = name.to_owned();
    raw_tool(
        def(name),
        Arc::new(FnTool(move |input: Value| {
            inputs.lock().unwrap().push((tool.clone(), input));
            Ok(ToolOutput::Structured(output.as_object().cloned().unwrap()))
        })),
    )
    .unwrap()
}

fn parcels() -> Value {
    json!({
        "total": 2,
        "activeCount": 1,
        "deliveries": [
            {"trackingNumber": "1Z999", "statusLabel": "In transit", "active": true},
            {"trackingNumber": "9400111", "statusLabel": "Delivered", "active": false},
        ],
        "cache": {"fetchedAt": null},
    })
}

struct Harness {
    _db: TestStore,
    store: Store,
    router: Router,
    inputs: Inputs,
}

async fn harness(clock: &Arc<TestClock>) -> Harness {
    let db = test_store(clock).await;
    let inputs: Inputs = Arc::default();
    let mut own = vec![
        fake("parcels_list", &inputs, parcels()),
        fake(
            "parcels_get",
            &inputs,
            json!({"delivery": {"trackingNumber": "1Z999", "statusLabel": "In transit"}, "submitted": null, "cache": {}}),
        ),
        raw_tool(
            def("email_health"),
            Arc::new(FnTool(|_: Value| {
                Err(ToolError::execute("IMAP is not configured"))
            })),
        )
        .unwrap(),
    ];
    let mut base = package_tools(&own);
    base.extend(own.iter().cloned());
    own.extend(
        shell_tools(
            &base,
            ShellDeps {
                store: db.store.clone(),
                clock: clock.clone(),
                tracker: tokio_util::task::TaskTracker::new(),
            },
        )
        .unwrap(),
    );
    let router = mcp_router(&db.store, clock, own, None);
    Harness {
        store: db.store.clone(),
        _db: db,
        router,
        inputs,
    }
}

async fn shell(router: &Router, arguments: Value) -> Value {
    let message = call_tool(router, "shell", arguments).await;
    assert!(!is_error(&message), "shell failed: {message}");
    result(&message).clone()
}

fn stdout(result: &Value) -> &str {
    result["structuredContent"]["stdout"].as_str().unwrap()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

async fn runs(store: &Store) -> Vec<ShellRunData> {
    store
        .read(|docs| docs.get_all::<ShellRunData>())
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_command_runs_its_tool_with_flags_and_prints_json_for_the_pipeline() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "parcels_get --tracking-number 1Z999 | jq -r .delivery.statusLabel"}),
    )
    .await;
    assert_eq!(stdout(&out), "In transit\n");
    assert_eq!(text(&out), "In transit\n");
    assert_eq!(out["structuredContent"]["exitCode"], json!(0));
    assert_eq!(
        h.inputs.lock().unwrap().clone(),
        [("parcels_get".to_owned(), json!({"trackingNumber": "1Z999"}))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_json_argument_is_the_whole_input_and_defaults_apply() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "parcels_list '{\"filter\":\"all\"}' >/dev/null; parcels_list --limit 5 --events=1 >/dev/null; parcels_list --events many"}),
    )
    .await;
    assert_eq!(out["structuredContent"]["exitCode"], json!(2));
    let inputs = h.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].1["filter"], json!("all"));
    assert_eq!(inputs[1].1["limit"], json!(5));
    assert_eq!(inputs[1].1["events"], json!(1));
    assert_eq!(inputs[1].1["filter"], json!("active"));
}

#[tokio::test(start_paused = true)]
async fn argument_free_tools_are_files_read_once_per_call() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "ls /omni | grep parcels; jq '.total' /omni/parcels_list.json; jq -r '.deliveries[] | select(.active) | .trackingNumber' /omni/parcels_list.json"}),
    )
    .await;
    assert_eq!(stdout(&out), "parcels_list.json\n2\n1Z999\n");
    assert_eq!(h.inputs.lock().unwrap().len(), 1);
    shell(
        &h.router,
        json!({"script": "cat /omni/parcels_list.json >/dev/null"}),
    )
    .await;
    assert_eq!(h.inputs.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn only_read_only_tools_outside_the_stylist_are_commands() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "tools | grep -c parcels_; email_send --to a@b.c; echo \"send=$?\"; stylist_context_get --namespace real; echo \"stylist=$?\""}),
    )
    .await;
    let printed = stdout(&out);
    assert!(printed.starts_with("2\n"), "{printed}");
    assert!(printed.contains("send=127"), "{printed}");
    assert!(printed.contains("stylist=127"), "{printed}");
}

#[tokio::test(start_paused = true)]
async fn help_lists_flags_and_bad_flags_fail_with_usage() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(&h.router, json!({"script": "parcels_get --help"})).await;
    assert!(stdout(&out).contains("--tracking-number <text> (required)"));
    let out = shell(&h.router, json!({"script": "parcels_get --carrier ups"})).await;
    assert_eq!(out["structuredContent"]["exitCode"], json!(2));
    let stderr = out["structuredContent"]["stderr"].as_str().unwrap();
    assert!(stderr.contains("unknown flag --carrier"), "{stderr}");
    assert!(text(&out).ends_with("[exit 2]"), "{}", text(&out));
}

#[tokio::test(start_paused = true)]
async fn a_failing_tool_reports_on_stderr_and_the_script_continues() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "email_health || echo \"failed with $?\""}),
    )
    .await;
    assert_eq!(stdout(&out), "failed with 1\n");
    assert_eq!(
        out["structuredContent"]["stderr"],
        json!("email_health: IMAP is not configured\n")
    );
    let out = shell(&h.router, json!({"script": "cat /omni/email_health.json"})).await;
    assert_ne!(out["structuredContent"]["exitCode"], json!(0));
}

#[tokio::test(start_paused = true)]
async fn omni_is_read_only_and_tmp_is_scratch() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "echo x > /omni/new.json; echo \"omni=$?\"; parcels_list > p.json && jq .total p.json && pwd"}),
    )
    .await;
    assert_eq!(stdout(&out), "omni=1\n2\n/tmp\n");
}

// Real time: the store runs on its own thread, so while a call waits on it a
// paused runtime looks idle and auto-advances the test clock to the next
// timer, sometimes past `SESSION_IDLE_MS`, which evicts the session.
#[tokio::test]
async fn a_session_keeps_files_and_variables_and_a_fresh_shell_does_not() {
    let clock = clock();
    let h = harness(&clock).await;
    shell(
        &h.router,
        json!({"script": "export PICK=1Z999; parcels_list > all.json", "session": "s1"}),
    )
    .await;
    let out = shell(
        &h.router,
        json!({"script": "echo $PICK; jq .total all.json", "session": "s1"}),
    )
    .await;
    assert_eq!(stdout(&out), "1Z999\n2\n");
    let out = shell(
        &h.router,
        json!({"script": "test -e all.json || echo none"}),
    )
    .await;
    assert_eq!(stdout(&out), "none\n");
    let out = shell(
        &h.router,
        json!({"script": "test -e all.json || echo none", "session": "s2"}),
    )
    .await;
    assert_eq!(stdout(&out), "none\n");
}

#[tokio::test(start_paused = true)]
async fn long_output_is_cut_with_a_notice() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "for i in $(seq 1 7000); do echo 0123456789; done"}),
    )
    .await;
    let structured = &out["structuredContent"];
    assert_eq!(stdout(&out).len(), 60_000);
    assert_eq!(structured["truncated"], json!(true));
    assert!(
        structured["stderr"]
            .as_str()
            .unwrap()
            .contains("stdout was cut to 60000 of 77000 characters")
    );
    let run = runs(&h.store).await.pop().unwrap();
    assert_eq!(run.stdout_bytes, 77_000);
    assert!(run.truncated);
}

#[tokio::test(start_paused = true)]
async fn a_syntax_error_is_an_error_run() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(&h.router, json!({"script": "if then fi ("})).await;
    assert_ne!(out["structuredContent"]["exitCode"], json!(0));
    let run = runs(&h.store).await.pop().unwrap();
    assert!(
        matches!(run.outcome, ShellOutcome::Error | ShellOutcome::Failed),
        "{:?}",
        run.outcome
    );
}

#[tokio::test(start_paused = true)]
async fn every_run_is_recorded_and_summarized_against_direct_calls() {
    let clock = clock();
    let h = harness(&clock).await;
    let direct = call_tool(&h.router, "parcels_list", json!({})).await;
    assert!(!is_error(&direct));
    let out = shell(
        &h.router,
        json!({"script": "jq '.total' /omni/parcels_list.json; parcels_get --tracking-number 1Z999 >/dev/null", "session": "measure"}),
    )
    .await;
    let run_id = out["structuredContent"]["runId"].as_str().unwrap();
    let run = runs(&h.store).await.pop().unwrap();
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.outcome, ShellOutcome::Ok);
    assert_eq!(run.session.as_deref(), Some("measure"));
    assert_eq!(run.tool_calls, 2);
    assert_eq!(run.returned_bytes, 2);
    assert!(run.tool_bytes > 100, "{}", run.tool_bytes);
    let vias: Vec<(&str, &str)> = run
        .calls
        .iter()
        .map(|c| (c.tool.as_str(), c.via.as_str()))
        .collect();
    assert_eq!(vias, [("parcels_list", "file"), ("parcels_get", "command")]);

    let calls: Vec<McpCallData> = h
        .store
        .read(|docs| docs.get_all::<McpCallData>())
        .await
        .unwrap();
    let parcels = calls.iter().find(|c| c.tool == "parcels_list").unwrap();
    let direct_text = direct["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(parcels.result_bytes, Some(direct_text.len() as i64));
    // The shell measures a tool result as the bytes a direct call returns.
    assert_eq!(run.calls[0].bytes, direct_text.len() as i64);
    assert!(parcels.structured_bytes.unwrap() > 0);
    let shell_call = calls.iter().find(|c| c.tool == "shell").unwrap();
    assert_eq!(shell_call.result_bytes, Some(2));

    let usage = call_tool(&h.router, "shell_usage", json!({})).await;
    assert!(!is_error(&usage), "{usage}");
    let usage = &usage["result"]["structuredContent"];
    assert_eq!(usage["shell"]["runs"], json!(1));
    assert_eq!(usage["shell"]["sessions"], json!(1));
    assert_eq!(usage["shell"]["toolCalls"], json!(2));
    assert_eq!(usage["shell"]["returnedBytes"], json!(2));
    assert!(usage["shell"]["reduction"].as_f64().unwrap() > 0.9);
    assert_eq!(usage["direct"]["shellCalls"], json!(1));
    assert_eq!(usage["direct"]["byTool"][0]["tool"], json!("parcels_list"));
    assert!(
        usage["schemas"]["toolsListBytes"].as_i64().unwrap()
            > 20 * usage["schemas"]["shellToolBytes"].as_i64().unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn a_script_stopped_by_a_sandbox_limit_is_recorded_as_limited() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "echo start; while true; do :; done; echo late", "session": "busy"}),
    )
    .await;
    assert_ne!(out["structuredContent"]["exitCode"], json!(0));
    assert!(!stdout(&out).contains("late"), "{}", stdout(&out));
    let run = runs(&h.store).await.pop().unwrap();
    assert_eq!(run.outcome, ShellOutcome::Limited, "{run:?}");
    let out = shell(
        &h.router,
        json!({"script": "echo again", "session": "busy"}),
    )
    .await;
    assert_eq!(stdout(&out), "again\n");
}

#[tokio::test(start_paused = true)]
async fn a_script_may_make_at_most_200_tool_calls() {
    let clock = clock();
    let h = harness(&clock).await;
    let out = shell(
        &h.router,
        json!({"script": "for i in $(seq 1 205); do parcels_get --tracking-number x >/dev/null 2>&1 && echo ok || echo refused; done | sort | uniq -c | tr -s ' '"}),
    )
    .await;
    assert_eq!(stdout(&out), " 200 ok\n 5 refused\n");
    assert_eq!(h.inputs.lock().unwrap().len(), 200);
    let run = runs(&h.store).await.pop().unwrap();
    assert_eq!(run.tool_calls, 200);
    assert_eq!(run.calls.len(), 50);
    assert_eq!(run.by_tool[0].calls, 200);
}
