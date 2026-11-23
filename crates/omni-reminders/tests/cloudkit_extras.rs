//! Verified list and recurrence operations.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use crate::common;

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use indexmap::IndexMap;
use omni_reminders::cloudkit::{ErrorCode, RemindersError};
use omni_reminders::cloudkit_extras::{ListQuery, RemindersCloudKitExtras};
use serde_json::{Map, Value, json};

const REMINDER_ID: &str = "Reminder/12345678";
const RULE_ID: &str = "RecurrenceRule/ABCDEFGH";
const LIST_ID: &str = "List/12345678";

fn reference(name: &str) -> Value {
    json!({"type": "REFERENCE", "value": {"recordName": name, "action": "VALIDATE"}})
}

fn int(value: i64) -> Value {
    json!({"type": "INT64", "value": value})
}

fn string(value: &str) -> Value {
    json!({"type": "STRING", "value": value})
}

type Acks = Box<dyn Fn(Vec<Value>) -> Vec<Value> + Send + Sync>;

#[derive(Default)]
struct Options {
    recurrence: bool,
    mutate_acks: Option<Acks>,
    lost_response: bool,
    stale_read: bool,
}

struct Fixture {
    client: RemindersCloudKitExtras,
    records: Arc<Mutex<IndexMap<String, Value>>>,
    writes: Arc<Mutex<Vec<Value>>>,
}

impl Fixture {
    fn record(&self, id: &str) -> Value {
        self.records.lock().unwrap()[id].clone()
    }

    fn set_field(&self, id: &str, field: &str, value: Value) {
        self.records.lock().unwrap()[id]["fields"][field] = value;
    }

    fn remove_field(&self, id: &str, field: &str) {
        if let Some(fields) = self.records.lock().unwrap()[id]["fields"].as_object_mut() {
            fields.remove(field);
        }
    }

    fn writes(&self) -> Vec<Value> {
        self.writes.lock().unwrap().clone()
    }
}

fn fixture(options: Options) -> Fixture {
    let mut records = IndexMap::new();
    records.insert(
        LIST_ID.to_owned(),
        json!({
            "recordName": LIST_ID,
            "recordType": "List",
            "recordChangeTag": "list-1",
            "fields": {
                "Name": string("Original"),
                "Color": string("blue"),
                "HiddenMetadata": int(7),
                "ResolutionTokenMap": string(&serde_json::to_string(&json!({
                    "extra": true,
                    "map": {
                        "name": {"counter": 3, "modificationTime": 5, "replicaID": "old"},
                        "color": {"counter": 2},
                    },
                })).unwrap()),
            },
        }),
    );
    records.insert(
        REMINDER_ID.to_owned(),
        json!({
            "recordName": REMINDER_ID,
            "recordType": "Reminder",
            "recordChangeTag": "reminder-1",
            "fields": {
                "List": reference(LIST_ID),
                "RecurrenceRuleIDs": {"type": "STRING_LIST", "value": if options.recurrence { json!(["ABCDEFGH"]) } else { json!([]) }},
                "NotesDocument": string("untouched"),
                "ResolutionTokenMap": string(r#"{"map":{"titleDocument":{"counter":5}}}"#),
            },
        }),
    );
    if options.recurrence {
        records.insert(
            RULE_ID.to_owned(),
            json!({
                "recordName": RULE_ID,
                "recordType": "RecurrenceRule",
                "recordChangeTag": "rule-1",
                "fields": {
                    "Reminder": reference(REMINDER_ID),
                    "Frequency": int(0),
                    "Interval": int(1),
                    "FirstDayOfTheWeek": int(0),
                    "Imported": int(0),
                    "Deleted": int(0),
                },
            }),
        );
    }
    let records = Arc::new(Mutex::new(records));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let next_tag = Arc::new(Mutex::new(0));
    let options = Arc::new(options);
    let (r, w, o, t) = (records.clone(), writes.clone(), options.clone(), next_tag);
    let post = Arc::new(
        move |path: &str, body: Value| -> BoxFuture<'static, Result<Value, RemindersError>> {
            let result = if path == "/records/lookup" {
                let id = body["records"][0]["recordName"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let record = r.lock().unwrap().get(&id).cloned();
                Ok(
                    json!({"records": [record.unwrap_or_else(|| json!({"recordName": id, "serverErrorCode": "NOT_FOUND"}))]}),
                )
            } else {
                w.lock().unwrap().push(body.clone());
                let acks: Vec<Value> = body["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|op| {
                        let mut record = op["record"].clone();
                        let mut tag = t.lock().unwrap();
                        *tag += 1;
                        record["recordChangeTag"] = json!(format!("ack-{tag}"));
                        let fields: Map<String, Value> = record["fields"]
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(name, field)| {
                                let mut field = field.clone();
                                if field.get("type").is_none() {
                                    field["type"] = json!(if name == "EndDate" {
                                        "TIMESTAMP"
                                    } else {
                                        "BYTES"
                                    });
                                }
                                (name.clone(), field)
                            })
                            .collect();
                        record["fields"] = Value::Object(fields);
                        record
                    })
                    .collect();
                if !o.stale_read {
                    let mut store = r.lock().unwrap();
                    for ack in &acks {
                        let name = ack["recordName"].as_str().unwrap().to_owned();
                        let mut merged = store
                            .get(&name)
                            .and_then(|old| old["fields"].as_object().cloned())
                            .unwrap_or_default();
                        merged.extend(ack["fields"].as_object().unwrap().clone());
                        let mut next = ack.clone();
                        next["fields"] = Value::Object(merged);
                        store.insert(name, next);
                    }
                }
                if o.lost_response {
                    Err(RemindersError {
                        operation: "fixture".into(),
                        code: ErrorCode::Transport,
                    })
                } else {
                    Ok(json!({"records": match &o.mutate_acks {
                        Some(mutate) => mutate(acks),
                        None => acks,
                    }}))
                }
            };
            Box::pin(async move { result })
        },
    );
    let query_records = records.clone();
    let query: ListQuery = Arc::new(move |_list: &str| {
        let rules: Vec<Value> = query_records
            .lock()
            .unwrap()
            .values()
            .filter(|r| r["recordType"] == "RecurrenceRule")
            .cloned()
            .collect();
        Box::pin(async move { Ok(rules) })
    });
    Fixture {
        client: RemindersCloudKitExtras::new(post, omni_testkit::test_clock(common::NOW), query),
        records,
        writes,
    }
}

fn fails<T: std::fmt::Debug>(result: Result<T, RemindersError>, expected: Option<ErrorCode>) {
    let error = result.expect_err("expected failure");
    if let Some(code) = expected {
        assert_eq!(error.code, code, "{error}");
    }
}

fn b64(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(text)
}

#[tokio::test]
async fn writes_new_selectors_without_guessing_types_preserves_observed_wrappers_on_update_and_verifies_values()
 {
    let x = fixture(Options::default());
    let created = x
        .client
        .create_recurrence(
            REMINDER_ID,
            "reminder-1",
            RULE_ID,
            &json!({
                "frequency": "monthly",
                "interval": 1,
                "endDate": 1_800_000_000_000_i64,
                "daysOfWeek": [{"dayOfTheWeek": 2, "weekNumber": -1}],
                "monthsOfYear": [1, 12],
            }),
        )
        .await
        .unwrap();
    let fields = x.writes()[0]["operations"][1]["record"]["fields"].clone();
    assert_eq!(fields["EndDate"], json!({"value": 1_800_000_000_000_i64}));
    assert_eq!(fields["MonthsOfTheYear"], json!({"value": b64("[1,12]")}));
    let patch = json!({"endDate": null, "monthsOfYear": [2]});
    let updated = x
        .client
        .update_recurrence(
            REMINDER_ID,
            &created.reminder_change_tag,
            RULE_ID,
            created.rules[0].record_change_tag.as_deref().unwrap(),
            patch.as_object().unwrap(),
        )
        .await
        .unwrap();
    let fields = x.writes()[1]["operations"][1]["record"]["fields"].clone();
    assert_eq!(
        fields["EndDate"],
        json!({"type": "TIMESTAMP", "value": null})
    );
    assert_eq!(
        fields["MonthsOfTheYear"],
        json!({"type": "BYTES", "value": b64("[2]")})
    );
    let rule = serde_json::to_value(&updated.rules[0]).unwrap();
    assert_eq!(rule["writable"], json!(true));
    assert_eq!(rule["recurrence"]["supported"], json!(true));
    assert_eq!(rule["recurrence"]["rule"]["endDate"], Value::Null);
    assert_eq!(rule["recurrence"]["rule"]["monthsOfYear"], json!([2]));
    assert_eq!(
        rule["recurrence"]["rule"]["daysOfWeek"],
        json!([{"dayOfTheWeek": 2, "weekNumber": -1}])
    );
}

#[tokio::test]
async fn renames_only_name_and_merges_resolution_tokens_without_touching_list_metadata_or_reminders()
 {
    let x = fixture(Options::default());
    let reminder = x.record(REMINDER_ID);
    let result = x
        .client
        .update_list(LIST_ID, "list-1", "Renamed")
        .await
        .unwrap();
    assert_eq!(result.title, "Renamed");
    assert_eq!(result.color.as_deref(), Some("blue"));
    assert_eq!(result.record_change_tag.as_deref(), Some("ack-1"));
    let writes = x.writes();
    assert_eq!(writes[0]["atomic"], json!(true));
    let mut keys: Vec<&String> = writes[0]["operations"][0]["record"]["fields"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    keys.sort();
    assert_eq!(keys, ["Name", "ResolutionTokenMap"]);
    let persisted = x.record(LIST_ID);
    assert_eq!(persisted["fields"]["HiddenMetadata"], int(7));
    let tokens: Value = serde_json::from_str(
        persisted["fields"]["ResolutionTokenMap"]["value"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(tokens["extra"], json!(true));
    assert_eq!(tokens["map"]["name"]["counter"], json!(4));
    assert_eq!(tokens["map"]["color"]["counter"], json!(2));
    assert_eq!(x.record(REMINDER_ID), reminder);
}

#[tokio::test]
async fn rejects_list_conflicts_and_malformed_token_maps_before_writing() {
    let x = fixture(Options::default());
    fails(
        x.client.update_list(LIST_ID, "wrong", "Renamed").await,
        Some(ErrorCode::Conflict),
    );
    x.set_field(LIST_ID, "ResolutionTokenMap", string("not json"));
    fails(
        x.client.update_list(LIST_ID, "list-1", "Renamed").await,
        None,
    );
    assert!(x.writes().is_empty());
}

#[tokio::test]
async fn reads_exact_recurrence_details_including_opaque_first_day_zero_and_unsupported_future_rules()
 {
    let x = fixture(Options {
        recurrence: true,
        ..Options::default()
    });
    let result =
        serde_json::to_value(x.client.get_recurrences(REMINDER_ID).await.unwrap()).unwrap();
    let rule = &result["rules"][0];
    assert_eq!(rule["id"], RULE_ID);
    assert_eq!(rule["reminderId"], REMINDER_ID);
    assert_eq!(rule["recordChangeTag"], "rule-1");
    assert_eq!(rule["writable"], json!(true));
    assert_eq!(
        rule["recurrence"],
        json!({"supported": true, "rule": {"frequency": "daily", "interval": 1, "firstDayOfWeek": 0}})
    );
    x.set_field(RULE_ID, "Frequency", int(100));
    let result =
        serde_json::to_value(x.client.get_recurrences(REMINDER_ID).await.unwrap()).unwrap();
    assert_eq!(result["rules"][0]["writable"], json!(false));
    assert_eq!(result["rules"][0]["recurrence"]["supported"], json!(false));
    assert_eq!(
        result["rules"][0]["recurrence"]["reason"],
        "unknown_frequency"
    );
}

#[tokio::test]
async fn creates_atomically_using_stable_child_identity_and_verifies_both_links_and_fields() {
    let x = fixture(Options::default());
    let result = x
        .client
        .create_recurrence(
            REMINDER_ID,
            "reminder-1",
            RULE_ID,
            &json!({"frequency": "daily", "interval": 2, "firstDayOfWeek": 0}),
        )
        .await
        .unwrap();
    let writes = x.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["atomic"], json!(true));
    let parent = &writes[0]["operations"][0]["record"];
    let child = &writes[0]["operations"][1]["record"];
    assert_eq!(
        parent["fields"]["RecurrenceRuleIDs"],
        json!({"type": "STRING_LIST", "value": ["ABCDEFGH"]})
    );
    assert_eq!(child["parent"], json!({"recordName": REMINDER_ID}));
    assert_eq!(child["fields"]["Frequency"], int(0));
    assert_eq!(child["fields"]["Interval"], int(2));
    assert_eq!(child["fields"]["Reminder"], reference(REMINDER_ID));
    assert_eq!(result.rules[0].id, RULE_ID);
    assert_eq!(result.rules[0].record_change_tag.as_deref(), Some("ack-2"));
    assert!(result.rules[0].writable);
    assert_eq!(
        x.record(REMINDER_ID)["fields"]["NotesDocument"],
        string("untouched")
    );
}

#[tokio::test]
async fn updates_using_both_change_tags_then_unlinks_and_soft_deletes_without_completing_the_reminder()
 {
    let x = fixture(Options {
        recurrence: true,
        ..Options::default()
    });
    let patch = json!({"frequency": "weekly", "interval": 3});
    let updated = x
        .client
        .update_recurrence(
            REMINDER_ID,
            "reminder-1",
            RULE_ID,
            "rule-1",
            patch.as_object().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&updated.rules[0].recurrence).unwrap(),
        json!({"supported": true, "rule": {"frequency": "weekly", "interval": 3, "firstDayOfWeek": 0}})
    );
    let removed = x
        .client
        .remove_recurrence(
            REMINDER_ID,
            &updated.reminder_change_tag,
            RULE_ID,
            updated.rules[0].record_change_tag.as_deref().unwrap(),
        )
        .await
        .unwrap();
    assert!(removed.rules.is_empty());
    assert_eq!(x.record(RULE_ID)["fields"]["Deleted"], int(1));
    assert_eq!(
        x.record(REMINDER_ID)["fields"]["RecurrenceRuleIDs"]["value"],
        json!([])
    );
    assert!(x.record(REMINDER_ID)["fields"].get("Completed").is_none());
}

#[tokio::test]
async fn rejects_missing_duplicate_foreign_error_and_untagged_mutation_acknowledgments() {
    for kind in ["missing", "duplicate", "foreign", "error", "no-tag"] {
        let mutate: Acks = Box::new(move |acks: Vec<Value>| match kind {
            "missing" => acks[..1].to_vec(),
            "duplicate" => vec![acks[0].clone(), acks[0].clone()],
            "foreign" => {
                let mut other = acks[1].clone();
                other["recordName"] = json!("other");
                vec![acks[0].clone(), other]
            }
            "error" => {
                let mut failed = acks[1].clone();
                failed["serverErrorCode"] = json!("PERMISSION_FAILURE");
                vec![acks[0].clone(), failed]
            }
            _ => {
                let mut untagged = acks[1].clone();
                untagged.as_object_mut().unwrap().remove("recordChangeTag");
                vec![acks[0].clone(), untagged]
            }
        });
        let x = fixture(Options {
            mutate_acks: Some(mutate),
            ..Options::default()
        });
        fails(
            x.client
                .create_recurrence(
                    REMINDER_ID,
                    "reminder-1",
                    RULE_ID,
                    &json!({"frequency": "daily", "interval": 1}),
                )
                .await,
            None,
        );
        assert_eq!(x.writes().len(), 1, "{kind}");
    }
}

#[tokio::test]
async fn treats_a_lost_response_or_failed_readback_as_failure_without_repeating_writes() {
    for options in [
        Options {
            lost_response: true,
            ..Options::default()
        },
        Options {
            stale_read: true,
            ..Options::default()
        },
    ] {
        let x = fixture(options);
        fails(
            x.client
                .create_recurrence(
                    REMINDER_ID,
                    "reminder-1",
                    RULE_ID,
                    &json!({"frequency": "daily", "interval": 1}),
                )
                .await,
            None,
        );
        assert_eq!(x.writes().len(), 1);
    }
}

#[tokio::test]
async fn rejects_unknown_rules_unlinked_relationships_wrong_owners_and_stale_tags_before_mutation()
{
    let x = fixture(Options {
        recurrence: true,
        ..Options::default()
    });
    let daily = json!({"frequency": "daily", "interval": 1});
    let interval = json!({"interval": 2});
    let patch = interval.as_object().unwrap();
    fails(
        x.client
            .create_recurrence(REMINDER_ID, "reminder-1", "RecurrenceRule/IJKLMNOP", &daily)
            .await,
        Some(ErrorCode::Unsupported),
    );
    fails(
        x.client
            .update_recurrence(REMINDER_ID, "reminder-1", RULE_ID, "wrong", patch)
            .await,
        Some(ErrorCode::Conflict),
    );
    x.set_field(RULE_ID, "FutureUnknownRule", string("opaque"));
    fails(
        x.client
            .update_recurrence(REMINDER_ID, "reminder-1", RULE_ID, "rule-1", patch)
            .await,
        Some(ErrorCode::Unsupported),
    );
    fails(
        x.client
            .remove_recurrence(REMINDER_ID, "reminder-1", RULE_ID, "rule-1")
            .await,
        Some(ErrorCode::Unsupported),
    );
    x.remove_field(RULE_ID, "FutureUnknownRule");
    x.set_field(
        REMINDER_ID,
        "RecurrenceRuleIDs",
        json!({"type": "STRING_LIST", "value": []}),
    );
    fails(
        x.client
            .remove_recurrence(REMINDER_ID, "reminder-1", RULE_ID, "rule-1")
            .await,
        Some(ErrorCode::Unsupported),
    );
    x.set_field(
        REMINDER_ID,
        "RecurrenceRuleIDs",
        json!({"type": "STRING_LIST", "value": ["ABCDEFGH"]}),
    );
    x.set_field(RULE_ID, "Reminder", reference("Reminder/FOREIGN1"));
    fails(x.client.get_recurrences(REMINDER_ID).await, None);
    assert!(x.writes().is_empty());
}
