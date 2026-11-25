//! The email MCP tools against their golden
//! metadata: inputs and outputs are validated by the golden JSON schemas.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_core::email::{EmailLink, EmailLinkMetadata, EmailLinkSource, ListUnsubscribe};
use omni_email::activity::{self, EmailActivityOutcome, EmailPipelineName, LlmCost, NewActivity};
use omni_email::mcp_tools::{EmailTools, tools};
use omni_email::retry;
use omni_mcp_kit::{McpTool, ToolContext, ToolOutput, ToolPhase};
use omni_runtime::ports::EmailFolderScope;
use omni_testkit::{TestApp, capture_logs};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use common::{FakeReader, email};

fn find<'a>(tools: &'a [McpTool], name: &str) -> &'a McpTool {
    tools.iter().find(|t| t.meta.name == name).unwrap()
}

async fn call(
    tools: &[McpTool],
    name: &str,
    input: Value,
) -> Result<Value, omni_mcp_kit::ToolError> {
    let cx = ToolContext {
        call_id: "1".to_owned(),
        cancel: CancellationToken::new(),
    };
    find(tools, name)
        .handler
        .call(input, cx)
        .await
        .map(|out| match out {
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            } => Value::Object(map),
        })
}

fn tool_set(app: &TestApp) -> Vec<McpTool> {
    tools(&EmailTools {
        store: app.ctx.store.clone(),
        ports: app.ctx.ports.clone(),
        config: app.ctx.config.clone(),
    })
    .unwrap()
}

#[tokio::test]
async fn registers_the_fourteen_tools_in_serving_order() {
    let app = TestApp::new().await;
    let names: Vec<String> = tool_set(&app).iter().map(|t| t.meta.name.clone()).collect();
    assert_eq!(
        names,
        [
            "email_search",
            "email_get",
            "email_thread_get",
            "email_health",
            "email_activity_list",
            "email_activity_get",
            "email_reprocess",
            "email_rules_list",
            "email_rules_upsert",
            "email_rules_delete",
            "email_feedback_list",
            "email_feedback_set",
            "email_retry_list",
            "email_retry_clear",
        ]
    );
}

#[tokio::test]
async fn mailbox_tools_need_an_active_reader() {
    let app = TestApp::new().await;
    let tools = tool_set(&app);
    let error = call(&tools, "email_get", json!({"emailId": "x"}))
        .await
        .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Execute);
    assert_eq!(error.message, "Email monitoring is not active");

    let health = call(&tools, "email_health", json!({})).await.unwrap();
    assert_eq!(health["monitoring"]["active"], false);
    assert_eq!(
        health["smtp"],
        json!({"configured": true, "configuredFrom": true, "provider": "smtp"})
    );
    assert_eq!(
        health["caldav"],
        json!({"configured": false, "provider": null})
    );

    let mut fetched = email("<m@x>");
    fetched.text_body = "x".repeat(30);
    fetched.to = Some(vec!["me@example.com".to_owned()]);
    app.ctx
        .ports
        .set_email_reader(FakeReader::new(move |_| Ok(Some(fetched.clone()))))
        .ok()
        .unwrap();
    let got = call(
        &tools,
        "email_get",
        json!({"emailId": "<m@x>", "bodyChars": 10}),
    )
    .await
    .unwrap();
    assert_eq!(got["email"]["excerpt"], "xxxxxxxxxx");
    assert_eq!(got["email"]["excerptTruncated"], true);
    assert_eq!(got["email"]["to"], json!(["me@example.com"]));
    assert_eq!(got["email"]["origin"], Value::Null);
    assert_eq!(got["email"]["linkMetadata"], Value::Null);

    let found = call(
        &tools,
        "email_search",
        json!({"from": "  shop  ", "folder": "inbox"}),
    )
    .await
    .unwrap();
    assert_eq!(found, json!({"items": [], "count": 0}));
    let error = call(
        &tools,
        "email_search",
        json!({"since": "2026-02-01T00:00:00Z", "before": "2026-01-01T00:00:00Z"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.message, "before must be later than since");
    let error = call(&tools, "email_search", json!({"query": "   "}))
        .await
        .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Input);
}

#[tokio::test]
async fn activity_feedback_and_retry_tools_round_trip() {
    let app = TestApp::new().await;
    let tools = tool_set(&app);
    activity::record(
        &app.ctx.store,
        NewActivity {
            detail: Some(String::new()),
            cost_cents: LlmCost::Cents(0.5),
            items: Some(vec!["1Z (ups): submitted".to_owned()]),
            ..NewActivity::new(
                EmailPipelineName::ParcelTracker,
                &email("m1"),
                EmailActivityOutcome::Processed,
            )
        },
    )
    .await
    .unwrap();
    let page = call(&tools, "email_activity_list", json!({}))
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["nextCursor"], Value::Null);
    assert_eq!(page["items"][0]["detail"], Value::Null);
    assert_eq!(page["items"][0]["costCents"], 0.5);

    let got = call(
        &tools,
        "email_activity_get",
        json!({"activityId": "ParcelTracker#m1"}),
    )
    .await
    .unwrap();
    assert_eq!(got["logs"], json!([]));
    assert_eq!(got["logsTruncated"], false);
    let missing = call(&tools, "email_activity_get", json!({"activityId": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(missing.message, "Unknown email activity: nope");

    let set = call(
        &tools,
        "email_feedback_set",
        json!({"activityId": "ParcelTracker#m1", "verdict": "missed", "note": "  "}),
    )
    .await
    .unwrap();
    assert_eq!(set["feedback"]["note"], Value::Null);
    let listed = call(
        &tools,
        "email_feedback_list",
        json!({"pipeline": "ParcelTracker"}),
    )
    .await
    .unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    let cleared = call(
        &tools,
        "email_feedback_set",
        json!({"activityId": "ParcelTracker#m1", "verdict": null}),
    )
    .await
    .unwrap();
    assert_eq!(cleared, json!({"feedback": null}));

    retry::enqueue(&app.ctx.store, "ParcelTracker", "m1", "503")
        .await
        .unwrap();
    let retries = call(
        &tools,
        "email_retry_list",
        json!({"pipeline": "ParcelTracker"}),
    )
    .await
    .unwrap();
    assert_eq!(retries["items"][0]["retryKey"], "ParcelTracker#m1");
    assert!(retries["items"][0].get("enqueueCount").is_none());
    let out = call(
        &tools,
        "email_retry_clear",
        json!({"pipeline": "ParcelTracker", "emailId": "m1"}),
    )
    .await
    .unwrap();
    assert_eq!(out, json!({"cleared": true}));
    let out = call(
        &tools,
        "email_retry_clear",
        json!({"pipeline": "ParcelTracker", "emailId": "m1"}),
    )
    .await
    .unwrap();
    assert_eq!(out, json!({"cleared": false}));

    let reprocess = call(
        &tools,
        "email_reprocess",
        json!({"activityId": "ParcelTracker#m1"}),
    )
    .await
    .unwrap_err();
    assert_eq!(reprocess.message, "Email monitoring is not active");
}

#[tokio::test]
async fn rule_tools_report_statuses() {
    let app = TestApp::new().await;
    let tools = tool_set(&app);
    let created = call(
        &tools,
        "email_rules_upsert",
        json!({"pattern": "plex.tv", "scope": "both", "verdict": "block"}),
    )
    .await
    .unwrap();
    assert_eq!(created["status"], "created");
    assert_eq!(created["rule"]["ruleId"], "both:@plex.tv");
    let exists = call(
        &tools,
        "email_rules_upsert",
        json!({"pattern": "@plex.tv", "scope": "parcel", "verdict": "block"}),
    )
    .await
    .unwrap();
    assert_eq!(exists["status"], "exists");
    let builtin = call(
        &tools,
        "email_rules_upsert",
        json!({"pattern": "noreply@github.com", "scope": "both", "verdict": "block"}),
    )
    .await
    .unwrap();
    assert_eq!(builtin, json!({"status": "builtin", "rule": null}));
    let listed = call(&tools, "email_rules_list", json!({})).await.unwrap();
    assert_eq!(listed["rules"].as_array().unwrap().len(), 1);
    let deleted = call(
        &tools,
        "email_rules_delete",
        json!({"ruleId": "both:@plex.tv"}),
    )
    .await
    .unwrap();
    assert_eq!(deleted, json!({"deleted": true}));
}

#[tokio::test]
async fn bounds_email_body_output_and_returns_useful_tool_errors() {
    let app = TestApp::new().await;
    let tools = tool_set(&app);
    let mut message = email("message-1");
    message.subject = "Bounded message".to_owned();
    message.from = "sender@example.test".to_owned();
    message.message_id = Some("<bounded@example.test>".to_owned());
    message.text_body = "x".repeat(50_000);
    let metadata = EmailLinkMetadata {
        links: vec![EmailLink {
            url: "https://example.test/unsubscribe?token=private-test".to_owned(),
            label: "Unsubscribe".to_owned(),
            source: EmailLinkSource::Html,
        }],
        links_truncated: false,
        list_unsubscribe: ListUnsubscribe {
            urls: vec!["mailto:leave@example.test".to_owned()],
            post: None,
            present: true,
            truncated: false,
        },
    };
    message.link_metadata = Some(metadata.clone());
    let reader = FakeReader::new(move |id| Ok((id == "message-1").then(|| message.clone())));
    app.ctx.ports.set_email_reader(reader.clone()).ok().unwrap();

    let logs = capture_logs();
    let bounded = call(
        &tools,
        "email_get",
        json!({"emailId": "message-1", "bodyChars": 1000}),
    )
    .await
    .unwrap();
    let excerpt = bounded["email"]["excerpt"].as_str().unwrap();
    assert_eq!(excerpt.chars().count(), 1000);
    assert_eq!(bounded["email"]["excerptTruncated"], true);
    assert_eq!(bounded["email"]["from"], "sender@example.test");
    assert_eq!(bounded["email"]["messageId"], "<bounded@example.test>");
    assert_eq!(
        bounded["email"]["linkMetadata"],
        serde_json::to_value(&metadata).unwrap()
    );
    assert!(
        logs.events()
            .iter()
            .all(|e| !e.message.contains("private-test")),
        "a link token reached the logs"
    );

    call(
        &tools,
        "email_get",
        json!({"emailId": "message-1", "fresh": true}),
    )
    .await
    .unwrap();
    let missing = call(&tools, "email_get", json!({"emailId": "missing"}))
        .await
        .unwrap_err();
    assert_eq!(missing.phase, ToolPhase::Execute);
    assert_eq!(
        missing.message,
        "Email no longer exists in the monitored mailbox"
    );
    assert_eq!(
        *reader.fetches.lock().unwrap(),
        [
            ("message-1".to_owned(), false),
            ("message-1".to_owned(), true),
            ("missing".to_owned(), false),
        ]
    );
}

#[tokio::test]
async fn browses_recent_inbox_mail_without_invented_criteria_and_forwards_freshness() {
    let app = TestApp::new().await;
    let tools = tool_set(&app);
    let reader = FakeReader::new(|_| Ok(None));
    app.ctx.ports.set_email_reader(reader.clone()).ok().unwrap();
    let result = call(
        &tools,
        "email_search",
        json!({"folder": "inbox", "fresh": true, "limit": 5}),
    )
    .await
    .unwrap();
    assert_eq!(result, json!({"items": [], "count": 0}));
    let searches = reader.searches.lock().unwrap();
    let [search] = searches.as_slice() else {
        panic!("expected one search, got {}", searches.len());
    };
    assert_eq!(search.folder, Some(EmailFolderScope::Inbox));
    assert!(search.fresh);
    assert_eq!(search.limit, 5);
    assert_eq!(
        (&search.query, &search.from, &search.to, &search.subject),
        (&None, &None, &None, &None)
    );
    assert_eq!(
        (search.unread, search.since_ms, search.before_ms),
        (None, None, None)
    );
}

#[tokio::test]
async fn thread_get_returns_the_whole_conversation_oldest_first() {
    let app = TestApp::new().await;
    let message = |id: &str, at: &str, in_reply_to: Option<&str>, refs: &[&str]| {
        let mut e = email(id);
        e.message_id = Some(id.to_owned());
        e.received_at = at.to_owned();
        e.in_reply_to = in_reply_to.map(str::to_owned);
        e.references = (!refs.is_empty()).then(|| refs.iter().map(|r| (*r).to_owned()).collect());
        e
    };
    let root = message("<root@x>", "2026-11-01T10:00:00.000Z", None, &[]);
    let middle = message(
        "<middle@x>",
        "2026-11-02T10:00:00.000Z",
        Some("<root@x>"),
        &["<root@x>"],
    );
    let reply = message(
        "<reply@x>",
        "2026-11-03T10:00:00.000Z",
        Some("<middle@x>"),
        &["<root@x>", "<middle@x>"],
    );
    // The server's References search is a substring match; this one only
    // looks related.
    let unrelated = message(
        "<other@x>",
        "2026-11-04T10:00:00.000Z",
        None,
        &["<root@x.other>"],
    );
    let (r, m) = (root.clone(), middle.clone());
    let mut reader = FakeReader::new(move |id| {
        Ok(match id {
            "<root@x>" => Some(r.clone()),
            "<middle@x>" => Some(m.clone()),
            _ => None,
        })
    });
    std::sync::Arc::get_mut(&mut reader).unwrap().search_results = vec![reply.clone(), unrelated];
    app.ctx.ports.set_email_reader(reader.clone()).ok().unwrap();
    let tools = tool_set(&app);

    let thread = call(&tools, "email_thread_get", json!({"emailId": "<middle@x>"}))
        .await
        .unwrap();
    let ids: Vec<&str> = thread["emails"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["messageId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["<root@x>", "<middle@x>", "<reply@x>"]);
    assert_eq!(thread["rootMessageId"], "<root@x>");
    assert_eq!(thread["count"], 3);
    // Replies were searched by References in the usual folders and in Sent.
    let searches = reader.searches.lock().unwrap().clone();
    assert!(
        searches
            .iter()
            .any(|s| s.references.as_deref() == Some("<root@x>")
                && s.folder == Some(EmailFolderScope::Sent))
    );

    let missing = call(&tools, "email_thread_get", json!({"emailId": "<gone@x>"}))
        .await
        .unwrap_err();
    assert_eq!(missing.code, "not_found");
}
