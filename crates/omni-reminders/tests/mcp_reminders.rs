//! The server Reminders MCP boundary. The tools run over a real
//! [`RemindersService`] whose CloudKit fake serves an 8-list fixture (CloudKit
//! already excludes soft-deleted reminders).
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

mod common;

use std::sync::Arc;

use futures::future::BoxFuture;
use omni_mcp_kit::{ApprovalPolicy, McpTool, ToolOutput, ToolPhase, registry::standalone_context};
use omni_reminders::apple::{AppleApi, AppleBeginResult, AppleRemindersError, CkPath};
use omni_reminders::config::RemindersConfiguration;
use omni_reminders::mcp::reminders_tools;
use omni_reminders::protected_access::ProtectedAccess;
use omni_reminders::service::{RemindersService, ServiceDeps};
use omni_reminders::store::{RemindersStore, RemindersStoreError, StoredState};
use serde_json::{Value, json};

struct Store(std::sync::Mutex<StoredState>);

impl RemindersStore for Store {
    fn read(&self) -> BoxFuture<'_, Result<StoredState, RemindersStoreError>> {
        let state = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(state) })
    }

    fn write(&self, state: StoredState) -> BoxFuture<'_, Result<(), RemindersStoreError>> {
        *self.0.lock().unwrap() = state;
        Box::pin(async { Ok(()) })
    }
}

/// Eight lists; list 0 has 8 incomplete reminders, the others 3, each plus one completed.
struct Apple;

fn list_record(index: usize) -> Value {
    json!({"recordName": format!("List/{index}"), "recordType": "List", "fields": {"Name": {"type": "STRING", "value": format!("List {index}")}}})
}

impl AppleApi for Apple {
    fn begin(&self) -> BoxFuture<'_, Result<AppleBeginResult, AppleRemindersError>> {
        Box::pin(async { Ok(AppleBeginResult::Ready) })
    }

    fn verify(&self) -> BoxFuture<'_, Result<bool, AppleRemindersError>> {
        Box::pin(async { Ok(true) })
    }

    fn submit_2fa<'a>(&'a self, _code: &'a str) -> BoxFuture<'a, Result<(), AppleRemindersError>> {
        Box::pin(async { Ok(()) })
    }

    fn request_pcs_access(&self) -> BoxFuture<'_, Result<ProtectedAccess, AppleRemindersError>> {
        Box::pin(async { Ok(ProtectedAccess::NotRequired) })
    }

    fn ck_post(
        &self,
        path: CkPath,
        body: Value,
    ) -> BoxFuture<'_, Result<Value, AppleRemindersError>> {
        let response = match path {
            CkPath::ChangesZone if body["zones"][0]["reverse"] == json!(true) => {
                json!({"zones": [{"records": []}]})
            }
            CkPath::ChangesZone => {
                json!({"zones": [{"records": (0..8).map(list_record).collect::<Vec<_>>(), "syncToken": "lists"}]})
            }
            _ => {
                let list_id = common::list_of_query(&body);
                let index: usize = list_id.trim_start_matches("List/").parse().unwrap();
                let incomplete = if index == 0 { 8 } else { 3 };
                let records: Vec<Value> = (0..=incomplete)
                    .map(|i| {
                        let mut r = common::record_fields(json!({
                            "TitleDocument": common::doc(&format!("Item {i}")),
                            "List": {"type": "REFERENCE", "value": {"recordName": list_id}},
                            "Completed": {"type": "INT64", "value": i64::from(i == incomplete)},
                        }));
                        r["recordName"] = json!(format!("Reminder/{list_id}/{i}"));
                        r
                    })
                    .collect();
                json!({"records": records})
            }
        };
        Box::pin(async move { Ok(response) })
    }
}

fn service(enabled: bool) -> RemindersService {
    let config = RemindersConfiguration {
        enabled: Some(if enabled { "true" } else { "false" }.into()),
        account: Some("test@example.com".into()),
        password: Some("never-sent".into()),
        storage_key: Some("a".repeat(64)),
        public_origin: Some("https://omni.example.test".into()),
        directory: "/tmp/omni-reminders-mcp-test-unused".into(),
    };
    RemindersService::new(
        &config,
        ServiceDeps {
            store: Arc::new(Store(std::sync::Mutex::new(StoredState::default()))),
            apple: Box::new(|_| Arc::new(Apple) as Arc<dyn AppleApi>),
            notify: Arc::new(|| Box::pin(async { Ok(()) })),
            log_failure: None,
            background: None,
            tracker: None,
            clock: omni_testkit::test_clock(common::NOW),
        },
    )
}

fn tool<'a>(tools: &'a [McpTool], name: &str) -> &'a McpTool {
    tools.iter().find(|t| t.meta.name == name).unwrap()
}

async fn execute(
    tools: &[McpTool],
    name: &str,
    input: Value,
) -> Result<Value, omni_mcp_kit::ToolError> {
    match tool(tools, name)
        .handler
        .call(input, standalone_context("test"))
        .await?
    {
        ToolOutput::Structured(map)
        | ToolOutput::Custom {
            structured: map, ..
        } => Ok(Value::Object(map)),
    }
}

#[tokio::test]
async fn keeps_list_counts_and_filtered_reminder_totals_independent_of_pagination() {
    let svc = service(true);
    svc.verify_access().await;
    let tools = reminders_tools(svc).unwrap();
    let first = execute(&tools, "list_reminder_lists", json!({"limit": 1}))
        .await
        .unwrap();
    assert_eq!(
        first,
        json!({"items": [{"id": "List/0", "title": "List 0", "color": null, "count": 8, "recordChangeTag": null}], "nextCursor": 1, "total": 8})
    );
    let last = execute(
        &tools,
        "list_reminder_lists",
        json!({"cursor": 7, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(last["items"][0]["id"], "List/7");
    assert_eq!(last["items"][0]["count"], 3);
    assert_eq!(
        (last["total"].clone(), last["nextCursor"].clone()),
        (json!(8), Value::Null)
    );
    for (input, total, next) in [
        (json!({"completed": false, "limit": 1}), 29, json!(1)),
        (
            json!({"listId": "List/0", "completed": false, "limit": 1}),
            8,
            json!(1),
        ),
        (
            json!({"listId": "List/0", "completed": true, "limit": 1}),
            1,
            Value::Null,
        ),
        (json!({"listId": "List/0", "limit": 1}), 9, json!(1)),
        (json!({"query": "ITEM 8", "limit": 1}), 1, Value::Null),
    ] {
        let page = execute(&tools, "list_reminders", input.clone())
            .await
            .unwrap();
        assert_eq!(
            (page["total"].clone(), page["nextCursor"].clone()),
            (json!(total), next),
            "{input}"
        );
    }
}

#[tokio::test]
async fn keeps_private_reads_disabled_when_the_service_is_absent() {
    let tools = reminders_tools(service(false)).unwrap();
    let error = execute(&tools, "list_reminders", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.message, "Reminders: disabled");
    assert_eq!(error.phase, ToolPhase::Execute);
}

#[tokio::test]
async fn rejects_recurrence_and_missing_mutation_identity_before_calling_a_service() {
    let tools = reminders_tools(service(false)).unwrap();
    for input in [
        json!({"id": "Reminder/test", "changeTag": "tag", "patch": {"title": "Changed"}}),
        json!({"id": "Reminder/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "patch": {"recurrence": "daily"}}),
        json!({"id": "Reminder/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "patch": {}}),
    ] {
        let error = execute(&tools, "update_reminder", input.clone())
            .await
            .unwrap_err();
        assert_eq!(error.phase, ToolPhase::Input, "{input}");
    }
}

#[tokio::test]
async fn bounds_search_and_result_pages_and_keeps_writes_approval_scoped() {
    let tools = reminders_tools(service(false)).unwrap();
    let error = execute(&tools, "list_reminders", json!({"limit": 101}))
        .await
        .unwrap_err();
    assert_eq!(error.phase, ToolPhase::Input);
    for name in [
        "create_reminder",
        "update_reminder",
        "complete_reminder",
        "reopen_reminder",
        "delete_reminder",
        "update_reminder_list",
        "create_reminder_recurrence",
        "update_reminder_recurrence",
        "remove_reminder_recurrence",
        "complete_recurring_reminder",
    ] {
        let meta = tool(&tools, name).meta;
        assert_eq!(
            meta.policy.recommended_policy,
            ApprovalPolicy::RequireApproval,
            "{name}"
        );
        assert!(!meta.annotations.read_only_hint, "{name}");
    }
}

#[tokio::test]
async fn rejects_unsupported_list_writes_and_missing_rule_concurrency_identity_at_the_mcp_boundary()
{
    let tools = reminders_tools(service(false)).unwrap();
    for (name, input) in [
        (
            "update_reminder_list",
            json!({"id": "List/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "color": "red"}),
        ),
        (
            "update_reminder_recurrence",
            json!({"id": "Reminder/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "ruleId": "RecurrenceRule/test", "patch": {"interval": 2}}),
        ),
        (
            "create_reminder_recurrence",
            json!({"id": "Reminder/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "rule": {"frequency": "not-a-frequency", "interval": 1}}),
        ),
        (
            "update_reminder_recurrence",
            json!({"id": "Reminder/test", "changeTag": "tag", "idempotencyKey": "fixture-key-123456", "ruleId": "RecurrenceRule/test", "ruleChangeTag": "r", "patch": {}}),
        ),
    ] {
        let error = execute(&tools, name, input.clone()).await.unwrap_err();
        assert_eq!(error.phase, ToolPhase::Input, "{name} {input}");
    }
    assert!(tools.iter().all(|t| t.meta.name != "create_reminder_list"));
    assert!(tools.iter().all(|t| t.meta.name != "delete_reminder_list"));
}

#[test]
fn exposes_exactly_the_reminders_tools_in_serving_order() {
    let tools = reminders_tools(service(false)).unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.meta.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "list_reminder_lists",
            "get_reminder_list",
            "update_reminder_list",
            "get_reminder_recurrence",
            "create_reminder_recurrence",
            "update_reminder_recurrence",
            "remove_reminder_recurrence",
            "complete_recurring_reminder",
            "list_reminders",
            "get_reminder",
            "create_reminder",
            "update_reminder",
            "complete_reminder",
            "reopen_reminder",
            "delete_reminder",
        ]
    );
}

#[tokio::test]
async fn returns_reminders_through_the_golden_output_schema() {
    let svc = service(true);
    svc.verify_access().await;
    let tools = reminders_tools(svc).unwrap();
    let page = execute(
        &tools,
        "list_reminders",
        json!({"listId": "List/1", "limit": 2}),
    )
    .await
    .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["items"][0]["title"], "Item 0");
    assert_eq!(page["items"][0]["priority"], 5);
}
