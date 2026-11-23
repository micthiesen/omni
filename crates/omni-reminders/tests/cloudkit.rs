//! The Reminders CloudKit codec.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use crate::common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use omni_reminders::cloudkit::{
    ErrorCode, Reminder, ReminderCreateFields, ReminderPatch, RemindersError,
};
use omni_reminders::codec::encode_crdt_document;
use omni_reminders::recurrence::Opt;
use serde_json::{Value, json};

fn code<T: std::fmt::Debug>(result: Result<T, RemindersError>) -> ErrorCode {
    result.expect_err("expected failure").code
}

fn flag_patch(flagged: bool) -> ReminderPatch {
    ReminderPatch {
        flagged: Some(flagged),
        ..ReminderPatch::default()
    }
}

/// `snapshotClient(response)`: list discovery returns `[list]`, queries return `records`.
fn snapshot_client(records: Value) -> omni_reminders::cloudkit::RemindersCloudKitClient {
    client(move |path, _| {
        Ok(if path == "/changes/zone" {
            json!({"zones": [{"records": [list()]}]})
        } else {
            json!({"records": records})
        })
    })
}

#[tokio::test]
async fn counts_incomplete_reminders_across_all_lists_instead_of_missing_or_stale_list_count_fields()
 {
    let lists: Vec<Value> = (0..8)
        .map(|index| {
            let mut fields = json!({"Name": list()["fields"]["Name"]});
            if index % 2 == 1 {
                fields["Count"] = json!({"type": "INT64", "value": 99});
            }
            json!({"recordName": format!("List/{index}"), "recordType": "List", "fields": fields})
        })
        .collect();
    let make = |list_id: &str, suffix: &str, completed: bool| {
        let mut r = record_fields(json!({
            "List": {"type": "REFERENCE", "value": {"recordName": list_id}},
            "Completed": {"type": "INT64", "value": i64::from(completed)},
        }));
        r["recordName"] = json!(format!("Reminder/{list_id}/{suffix}"));
        r
    };
    let c = client(move |path, body| {
        if path == "/changes/zone" {
            return Ok(json!({"zones": [{"records": lists, "syncToken": "initial"}]}));
        }
        let list_id = list_of_query(&body);
        if body.get("continuationMarker").is_none() {
            return Ok(json!({
                "records": [make(&list_id, "first", false), make(&list_id, "completed", true)],
                "continuationMarker": "second",
            }));
        }
        let mut hard = make(&list_id, "hard-deleted", false);
        hard["deleted"] = json!(true);
        Ok(json!({"records": [
            make(&list_id, "second", false),
            hard,
            {"recordName": format!("Reminder/{list_id}/soft-deleted"), "recordType": "Reminder", "fields": {"Deleted": {"type": "INT64", "value": 1}}},
            make("List/unknown", "outside-list", false),
        ]}))
    });
    let snapshot = c.read_snapshot().await.unwrap();
    assert_eq!(snapshot.lists.len(), 8);
    assert_eq!(
        snapshot.lists.iter().map(|l| l.count).collect::<Vec<_>>(),
        vec![2; 8]
    );
    let incomplete = snapshot
        .reminders
        .iter()
        .filter(|r| !r.completed && !r.deleted)
        .count();
    assert_eq!(
        snapshot.lists.iter().map(|l| l.count).sum::<u64>(),
        incomplete as u64
    );
}

#[tokio::test]
async fn recomputes_counts_from_atomic_deltas_for_completion_reopen_move_and_deletion_without_list_metadata_changes()
 {
    let mut other = list();
    other["recordName"] = json!("List/2");
    let changed = |fields: Value| record_fields(fields);
    let deltas = [
        json!([list(), other]),
        json!([changed(json!({"Completed": {"type": "INT64", "value": 1}}))]),
        json!([changed(json!({"Completed": {"type": "INT64", "value": 0}}))]),
        json!([changed(
            json!({"List": {"type": "REFERENCE", "value": {"recordName": "List/2"}}})
        )]),
        json!([{"recordName": "Reminder/12345678", "deleted": true}]),
    ];
    let page = AtomicUsize::new(0);
    let c = client(move |path, body| {
        if path == "/changes/zone" {
            let index = page.fetch_add(1, Ordering::SeqCst);
            return Ok(
                json!({"zones": [{"records": deltas[index], "syncToken": format!("cursor-{}", index + 1)}]}),
            );
        }
        Ok(
            json!({"records": if list_of_query(&body) == "List/1" { json!([record()]) } else { json!([]) }}),
        )
    });
    for expected in [[1, 0], [0, 0], [1, 0], [0, 1], [0, 0]] {
        let snapshot = c.read_snapshot().await.unwrap();
        assert_eq!(
            snapshot.lists.iter().map(|l| l.count).collect::<Vec<_>>(),
            expected
        );
    }
}

#[tokio::test]
async fn explicitly_invalidates_both_snapshot_and_list_cursors_before_a_fresh_full_read() {
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let queries = Arc::new(AtomicUsize::new(0));
    let scans = AtomicUsize::new(0);
    let (seen, counted) = (cursors.clone(), queries.clone());
    let c = client(move |path, body| {
        if path == "/records/query" {
            counted.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"records": [record()]}));
        }
        seen.lock().unwrap().push(sync_token(&body));
        let n = scans.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({"zones": [{"records": [list()], "syncToken": format!("complete-{n}")}]}))
    });
    c.read_snapshot().await.unwrap();
    assert!(c.has_snapshot());
    c.invalidate_snapshot();
    assert!(!c.has_snapshot());
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 1);
    assert!(c.has_snapshot());
    assert_eq!(*cursors.lock().unwrap(), vec![Value::Null, Value::Null]);
    assert_eq!(queries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bounds_incremental_page_traversal_without_advancing_the_prior_cursor() {
    let pages = Arc::new(AtomicUsize::new(0));
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let (counter, seen) = (pages.clone(), cursors.clone());
    let c = client(move |_, body| {
        seen.lock().unwrap().push(sync_token(&body));
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(
            json!({"zones": [{"records": [], "syncToken": format!("cursor-{n}"), "moreComing": n > 1 && n <= 201}]}),
        )
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(pages.load(Ordering::SeqCst), 201);
    c.read_snapshot().await.unwrap();
    assert_eq!(
        cursors.lock().unwrap().last().cloned(),
        Some(json!("cursor-1"))
    );
}

#[tokio::test]
async fn bounds_incremental_processed_records_and_keeps_the_prior_index() {
    let calls = AtomicUsize::new(0);
    let c = client(move |_, _| {
        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
        let records: Vec<Value> = if n == 2 {
            (0..10_001)
                .map(|i| json!({"recordName": format!("deleted-{i}"), "deleted": true}))
                .collect()
        } else {
            Vec::new()
        };
        Ok(json!({"zones": [{"records": records, "syncToken": format!("cursor-{n}")}]}))
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert!(c.has_snapshot());
    assert!(c.read_snapshot().await.unwrap().reminders.is_empty());
}

#[tokio::test]
async fn bounds_retained_index_records_across_otherwise_small_incremental_reads() {
    let changes = AtomicUsize::new(0);
    let c = client(move |path, _| {
        if path == "/records/query" {
            let records: Vec<Value> = (0..9_999)
                .map(|i| record_with(json!({"recordName": format!("Reminder/{i}")})))
                .collect();
            return Ok(json!({"records": records}));
        }
        let n = changes.fetch_add(1, Ordering::SeqCst) + 1;
        let records = match n {
            1 => json!([list()]),
            2 => json!([record_with(json!({"recordName": "Reminder/new"}))]),
            _ => json!([]),
        };
        Ok(json!({"zones": [{"records": records, "syncToken": format!("cursor-{n}")}]}))
    });
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 9_999);
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 9_999);
}

#[tokio::test]
async fn uses_atomic_deltas_after_a_complete_snapshot_including_new_lists_and_removals() {
    let calls = Calls::default();
    let log = calls.clone();
    let mut new_list = list();
    new_list["recordName"] = json!("List/new");
    let mut added =
        record_fields(json!({"List": {"type": "REFERENCE", "value": {"recordName": "List/new"}}}));
    added["recordName"] = json!("Reminder/new");
    let pages = [
        json!({"records": [list()], "syncToken": "before-queries"}),
        json!({"records": [new_list, added], "moreComing": true, "syncToken": "delta-page"}),
        json!({"records": [{"recordName": "List/1", "deleted": true}], "syncToken": "delta-complete"}),
        json!({"records": [{"recordName": "Reminder/new", "fields": {"Deleted": {"type": "INT64", "value": 1}}}], "syncToken": "deleted"}),
    ];
    let changes = AtomicUsize::new(0);
    let c = client(move |path, body| {
        log.push(path, &body);
        if path == "/records/query" {
            return Ok(json!({"records": [record()]}));
        }
        Ok(json!({"zones": [pages[changes.fetch_add(1, Ordering::SeqCst)]]}))
    });
    assert!(!c.has_snapshot());
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 1);
    assert!(c.has_snapshot());
    let refreshed = c.read_snapshot().await.unwrap();
    assert_eq!(
        refreshed
            .lists
            .iter()
            .map(|l| l.id.as_str())
            .collect::<Vec<_>>(),
        ["List/new"]
    );
    assert_eq!(
        refreshed
            .reminders
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        ["Reminder/new"]
    );
    let bodies = calls.bodies();
    assert_eq!(calls.paths()[2], "/changes/zone");
    assert_eq!(sync_token(&bodies[2]), json!("before-queries"));
    assert!(bodies[2]["zones"][0].get("desiredRecordTypes").is_none());
    assert_eq!(sync_token(&bodies[3]), json!("delta-page"));
    assert!(c.read_snapshot().await.unwrap().reminders.is_empty());
    assert_eq!(
        calls
            .paths()
            .iter()
            .filter(|p| *p == "/records/query")
            .count(),
        1
    );
}

#[tokio::test]
async fn applies_recurrence_additions_and_tombstones_without_querying_lists_again() {
    let pages = [
        json!({"records": [list()], "syncToken": "start"}),
        json!({"records": [rule_record("RecurrenceRule/1", "Reminder/12345678")], "syncToken": "recurring"}),
        json!({"records": [{"recordName": "RecurrenceRule/1", "deleted": true}], "syncToken": "ordinary"}),
    ];
    let changes = AtomicUsize::new(0);
    let c = client(move |path, _| {
        if path == "/records/query" {
            return Ok(json!({"records": [record()]}));
        }
        Ok(json!({"zones": [pages[changes.fetch_add(1, Ordering::SeqCst)]]}))
    });
    assert!(!c.read_snapshot().await.unwrap().reminders[0].recurring);
    assert!(c.read_snapshot().await.unwrap().reminders[0].recurring);
    assert!(!c.read_snapshot().await.unwrap().reminders[0].recurring);
}

#[tokio::test]
async fn retains_snapshot_records_and_cursor_after_a_malformed_delta() {
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let seen = cursors.clone();
    let pages = [
        json!({"records": [list()], "syncToken": "stable"}),
        json!({"records": [
            {"recordName": "Reminder/12345678", "deleted": true},
            {"recordName": "RecurrenceRule/bad", "recordType": "RecurrenceRule"},
        ], "syncToken": "bad"}),
        json!({"records": [], "syncToken": "recovered"}),
    ];
    let changes = AtomicUsize::new(0);
    let c = client(move |path, body| {
        if path == "/records/query" {
            return Ok(json!({"records": [record()]}));
        }
        seen.lock().unwrap().push(sync_token(&body));
        Ok(json!({"zones": [pages[changes.fetch_add(1, Ordering::SeqCst)]]}))
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert!(c.has_snapshot());
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 1);
    assert_eq!(
        *cursors.lock().unwrap(),
        vec![Value::Null, json!("stable"), json!("stable")]
    );
}

#[tokio::test]
async fn refreshes_recurrence_protection_from_the_index_before_a_mutation() {
    let calls = Calls::default();
    let log = calls.clone();
    let changes = AtomicUsize::new(0);
    let c = client(move |path, body| {
        log.push(path, &body);
        if path == "/records/query" || path == "/records/lookup" {
            return Ok(json!({"records": [record()]}));
        }
        let n = changes.fetch_add(1, Ordering::SeqCst) + 1;
        let records = if n == 1 {
            json!([list()])
        } else {
            json!([rule_record("RecurrenceRule/1", "Reminder/12345678")])
        };
        Ok(json!({"zones": [{"records": records, "syncToken": format!("cursor-{n}")}]}))
    });
    let current = c.read_snapshot().await.unwrap().reminders[0].clone();
    assert_eq!(
        code(c.update_reminder(&current, &flag_patch(false)).await),
        ErrorCode::Unsupported
    );
    assert_eq!(
        calls.paths(),
        [
            "/changes/zone",
            "/records/query",
            "/records/lookup",
            "/changes/zone"
        ]
    );
}

#[tokio::test]
async fn fails_closed_when_indexed_recurrence_refresh_cannot_confirm_a_looked_up_reminder() {
    let calls = Calls::default();
    let log = calls.clone();
    let changes = AtomicUsize::new(0);
    let c = client(move |path, body| {
        log.push(path, &body);
        if path == "/records/query" || path == "/records/lookup" {
            return Ok(json!({"records": [record()]}));
        }
        let n = changes.fetch_add(1, Ordering::SeqCst) + 1;
        let records = if n == 1 {
            json!([list()])
        } else {
            json!([{"recordName": "Reminder/12345678", "deleted": true}])
        };
        Ok(json!({"zones": [{"records": records, "syncToken": format!("cursor-{n}")}]}))
    });
    let current = c.read_snapshot().await.unwrap().reminders[0].clone();
    assert_eq!(code(c.delete_reminder(&current).await), ErrorCode::Protocol);
    assert!(!calls.paths().contains(&"/records/modify".to_owned()));
}

#[tokio::test]
async fn does_not_expose_an_index_from_an_incomplete_initial_query() {
    let c = client(|path, _| {
        Ok(if path == "/changes/zone" {
            json!({"zones": [{"records": [list()], "syncToken": "start"}]})
        } else {
            json!({"records": [{"recordName": "error", "serverErrorCode": "FAILED"}]})
        })
    });
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert!(!c.has_snapshot());
}

#[tokio::test]
async fn retains_the_prior_index_without_rescanning_when_an_incremental_response_has_no_cursor() {
    let changes = AtomicUsize::new(0);
    let queries = Arc::new(AtomicUsize::new(0));
    let counted = queries.clone();
    let c = client(move |path, _| {
        if path == "/records/query" {
            counted.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({"records": [record()]}));
        }
        let n = changes.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(json!({"zones": [match n {
            1 => json!({"records": [list()], "syncToken": "start"}),
            2 => json!({"records": []}),
            _ => json!({"records": [], "syncToken": "rebuilt"}),
        }]}))
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 1);
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    assert!(c.has_snapshot());
}

#[tokio::test]
async fn does_not_cache_a_full_snapshot_without_a_usable_sync_token() {
    let c = client(|_, _| Ok(json!({"zones": [{"records": []}]})));
    c.read_snapshot().await.unwrap();
    assert!(!c.has_snapshot());
}

#[tokio::test]
async fn rejects_cycling_incremental_cursors_without_advancing_the_saved_index() {
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let seen = cursors.clone();
    let pages = [
        json!({"records": [list()], "syncToken": "start"}),
        json!({"records": [], "moreComing": true, "syncToken": "next"}),
        json!({"records": [], "moreComing": true, "syncToken": "start"}),
        json!({"records": [], "syncToken": "done"}),
    ];
    let changes = AtomicUsize::new(0);
    let c = client(move |path, body| {
        if path == "/records/query" {
            return Ok(json!({"records": [record()]}));
        }
        seen.lock().unwrap().push(sync_token(&body));
        Ok(json!({"zones": [pages[changes.fetch_add(1, Ordering::SeqCst)]]}))
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    c.read_snapshot().await.unwrap();
    assert_eq!(
        *cursors.lock().unwrap(),
        vec![Value::Null, json!("start"), json!("next"), json!("start")]
    );
}

#[tokio::test]
async fn keeps_populated_malformed_or_unknown_recurrence_fields_read_only() {
    let cases = [
        (
            "RecurrenceRuleIDs",
            "STRING_LIST",
            json!(["RecurrenceRule/1"]),
        ),
        (
            "RecurrenceRuleIDs",
            "UNKNOWN_LIST",
            json!(["RecurrenceRule/1"]),
        ),
        ("RecurrenceRuleIDs", "STRING_LIST", Value::Null),
        ("RecurrenceRuleIDs", "UNKNOWN_LIST", json!("")),
        ("RecurrenceRuleIDs", "STRING", json!([])),
        ("RecurrenceRules", "UNKNOWN_LIST", json!([])),
    ];
    for (key, ty, value) in cases {
        let calls = Calls::default();
        let log = calls.clone();
        let fixture = record_fields(json!({key: {"type": ty, "value": value}}));
        let c = client(move |path, body| {
            log.push(path, &body);
            Ok(json!({"records": [fixture]}))
        });
        let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
        assert!(current.recurring, "{key} {ty} {value}");
        assert_eq!(
            code(c.update_reminder(&current, &flag_patch(false)).await),
            ErrorCode::Unsupported
        );
        assert_eq!(calls.paths(), ["/records/lookup"]);
    }
}

#[tokio::test]
async fn requests_at_most_50_reminders_on_every_compound_query_page() {
    let limits = Arc::new(Mutex::new(Vec::new()));
    let seen = limits.clone();
    let pages = AtomicUsize::new(0);
    let c = client(move |path, body| {
        if path == "/changes/zone" {
            return Ok(json!({"zones": [{"records": [list()]}]}));
        }
        seen.lock().unwrap().push(body["resultsLimit"].clone());
        let n = pages.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(if n < 3 {
            json!({"records": [], "continuationMarker": format!("page-{n}")})
        } else {
            json!({"records": []})
        })
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(*limits.lock().unwrap(), vec![json!(50); 3]);
}

#[tokio::test]
async fn caches_complete_list_discovery_and_preserves_it_after_an_incomplete_refresh() {
    let cursors = Arc::new(Mutex::new(Vec::new()));
    let seen = cursors.clone();
    let responses = [
        json!({"records": [list()], "syncToken": "stable"}),
        json!({"records": [{"recordName": "List/1", "deleted": true}], "moreComing": true, "syncToken": "partial"}),
        json!({"error": {"serverErrorCode": "ERROR"}}),
        json!({"records": [], "syncToken": "done"}),
        json!({"records": [{"recordName": "List/1", "recordType": "List", "fields": {"Deleted": {"type": "INT64", "value": 1}}}], "syncToken": "deleted"}),
    ];
    let reads = AtomicUsize::new(0);
    let c = client(move |path, body| {
        if path != "/changes/zone" {
            return Ok(json!({"records": []}));
        }
        seen.lock().unwrap().push(sync_token(&body));
        Ok(json!({"zones": [responses[reads.fetch_add(1, Ordering::SeqCst)]]}))
    });
    c.read_snapshot().await.unwrap();
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(c.read_snapshot().await.unwrap().lists.len(), 1);
    assert_eq!(c.read_snapshot().await.unwrap().lists.len(), 0);
    assert_eq!(
        *cursors.lock().unwrap(),
        vec![
            Value::Null,
            json!("stable"),
            json!("partial"),
            json!("stable"),
            json!("done")
        ]
    );
}

#[tokio::test]
async fn allows_long_list_history_without_consuming_the_current_record_query_budget() {
    let pages = Arc::new(AtomicUsize::new(0));
    let counter = pages.clone();
    let c = client(move |path, _| {
        Ok(if path == "/changes/zone" {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            json!({"zones": [{"records": if n == 82 { json!([list()]) } else { json!([]) }, "syncToken": format!("cursor-{n}"), "moreComing": n < 82}]})
        } else {
            json!({"records": [record()]})
        })
    });
    assert_eq!(c.read_snapshot().await.unwrap().reminders.len(), 1);
    assert_eq!(pages.load(Ordering::SeqCst), 82);
}

#[tokio::test]
async fn caps_incomplete_list_discovery_and_retries_from_its_original_cursor() {
    let pages = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(Value::Null));
    let (counter, cursor) = (pages.clone(), last.clone());
    let c = client(move |_, body| {
        *cursor.lock().unwrap() = sync_token(&body);
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(
            json!({"zones": [{"records": [], "syncToken": format!("cursor-{n}"), "moreComing": n <= 200}]}),
        )
    });
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(pages.load(Ordering::SeqCst), 200);
    assert!(c.read_snapshot().await.unwrap().lists.is_empty());
    assert_eq!(*last.lock().unwrap(), Value::Null);
}

#[tokio::test]
async fn probes_newest_reminders_without_scanning_the_entire_history_or_updating_the_list_cursor() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        let first = log.len() == 1;
        Ok(json!({"zones": [{
            "records": if first { json!([list(), record()]) } else { json!([]) },
            "moreComing": first,
            "syncToken": "probe-only",
        }]}))
    });
    c.verify_read_access().await.unwrap();
    let bodies = calls.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["zones"][0]["reverse"], json!(true));
    assert_eq!(
        bodies[0]["zones"][0]["desiredRecordTypes"],
        json!(["List", "Reminder"])
    );
    c.read_snapshot().await.unwrap();
    let bodies = calls.bodies();
    assert_eq!(bodies[1]["zones"][0]["desiredRecordTypes"], json!(["List"]));
    assert!(bodies[1]["zones"][0].get("syncToken").is_none());
}

#[tokio::test]
async fn does_not_use_list_names_or_a_single_readable_reminder_to_claim_protected_access() {
    let pages = Arc::new(AtomicUsize::new(0));
    let counter = pages.clone();
    let c = client(move |_, _| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let mut encrypted =
            record_fields(json!({"TitleDocument": {"type": "ENCRYPTED_BYTES", "value": "opaque"}}));
        encrypted["recordName"] = json!("encrypted");
        Ok(json!({"zones": [{
            "records": if n == 1 { json!([list()]) } else { json!([record(), encrypted]) },
            "moreComing": true,
            "syncToken": "next",
        }]}))
    });
    assert_eq!(
        code(c.verify_read_access().await),
        ErrorCode::AwaitingDeviceApproval
    );
    assert_eq!(pages.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn accepts_a_verified_empty_account_but_rejects_an_incomplete_access_probe() {
    let empty = client(|_, _| Ok(json!({"zones": [{"records": []}]})));
    empty.verify_read_access().await.unwrap();
    let incomplete =
        client(|_, _| Ok(json!({"zones": [{"records": [list()], "moreComing": true}]})));
    assert_eq!(
        code(incomplete.verify_read_access().await),
        ErrorCode::Protocol
    );
    let missing = client(|_, _| Ok(json!({"zones": [{}]})));
    assert_eq!(
        code(missing.verify_read_access().await),
        ErrorCode::Protocol
    );
}

#[tokio::test]
async fn pages_list_discovery_and_current_records_excluding_stale_deleted_and_other_list_reminders()
{
    let calls = Calls::default();
    let log = calls.clone();
    let stale = record_fields(json!({"TitleDocument": doc("Stale")}));
    let c = client(move |path, body| {
        log.push(path, &body);
        if path == "/changes/zone" {
            let first = log.len() == 1;
            return Ok(json!({"zones": [{
                "records": if first { json!([list(), stale]) } else { json!([]) },
                "moreComing": first,
                "syncToken": "lists-next",
            }]}));
        }
        Ok(if body.get("continuationMarker").is_some() {
            json!({"records": [record_fields(json!({"Completed": {"type": "INT64", "value": 1}}))], "continuationMarker": null})
        } else {
            let mut other = record_fields(
                json!({"List": {"type": "REFERENCE", "value": {"recordName": "List/other"}}}),
            );
            other["recordName"] = json!("Reminder/other");
            json!({"records": [
                other,
                {"recordName": "Reminder/deleted", "recordType": "Reminder", "deleted": true},
                {"recordName": "Reminder/softdeleted", "recordType": "Reminder", "fields": {"Deleted": {"type": "INT64", "value": 1}}},
            ], "continuationMarker": "query-next"})
        })
    });
    let result = c.read_snapshot().await.unwrap();
    assert_eq!(result.reminders.len(), 1);
    assert_eq!(result.reminders[0].title, "Buy milk");
    assert!(result.reminders[0].completed);
    let bodies = calls.bodies();
    assert_eq!(sync_token(&bodies[1]), json!("lists-next"));
    assert_eq!(bodies[1]["zones"][0]["desiredRecordTypes"], json!(["List"]));
    assert_eq!(bodies[3]["continuationMarker"], json!("query-next"));
}

#[tokio::test]
async fn rejects_a_looping_lists_or_query_cursor() {
    for kind in ["lists", "query"] {
        let pages = Arc::new(AtomicUsize::new(0));
        let counter = pages.clone();
        let c = client(move |path, _| {
            if path == "/changes/zone" && kind == "query" {
                return Ok(json!({"zones": [{"records": [list()]}]}));
            }
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let cursor = if n % 2 == 1 { "a" } else { "b" };
            Ok(if kind == "lists" {
                json!({"zones": [{"records": [], "moreComing": true, "syncToken": cursor}]})
            } else {
                json!({"records": [], "continuationMarker": cursor})
            })
        });
        assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
        assert_eq!(pages.load(Ordering::SeqCst), 3, "{kind}");
    }
}

#[tokio::test]
async fn enforces_one_page_budget_across_discovered_lists() {
    let queries = Arc::new(AtomicUsize::new(0));
    let counted = queries.clone();
    let c = client(move |path, _| {
        if path == "/changes/zone" {
            let lists: Vec<Value> = (0..201)
                .map(|i| {
                    let mut l = list();
                    l["recordName"] = json!(format!("List/{i}"));
                    l
                })
                .collect();
            return Ok(json!({"zones": [{"records": lists}]}));
        }
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"records": []}))
    });
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(queries.load(Ordering::SeqCst), 200);
}

#[tokio::test]
async fn enforces_one_record_budget_across_query_pages() {
    let queries = Arc::new(AtomicUsize::new(0));
    let counted = queries.clone();
    let c = client(move |path, _| {
        if path == "/changes/zone" {
            return Ok(json!({"zones": [{"records": [list()]}]}));
        }
        let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
        let records: Vec<Value> = (0..5_000)
            .map(|i| json!({"recordName": format!("Deleted/{i}"), "deleted": true}))
            .collect();
        let mut response = json!({"records": records});
        if n == 1 {
            response["continuationMarker"] = json!("next");
        }
        Ok(response)
    });
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
    assert_eq!(queries.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn checks_later_query_pages_for_recurrence_before_allowing_a_mutation() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        if path == "/records/lookup" {
            return Ok(json!({"records": [record()]}));
        }
        assert_eq!(body["query"]["filterBy"][0]["fieldName"], "List");
        assert_eq!(list_of_query(&body), "List/1");
        Ok(if body.get("continuationMarker").is_some() {
            json!({"records": [rule_record("RecurrenceRule/1", "Reminder/12345678")]})
        } else {
            json!({"records": [record()], "continuationMarker": "next"})
        })
    });
    let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
    assert!(current.recurring);
    assert_eq!(
        code(c.delete_reminder(&current).await),
        ErrorCode::Unsupported
    );
    assert_eq!(
        calls.paths(),
        ["/records/lookup", "/records/query", "/records/query"]
    );
}

#[tokio::test]
async fn rejects_uncertain_recurrence_query_responses_before_mutation() {
    let responses = [
        json!({}),
        json!({"records": [], "continuationMarker": 2}),
        json!({"records": [{"recordName": "error", "serverErrorCode": "UNKNOWN_ERROR"}]}),
        json!({"records": [{"recordName": "RecurrenceRule/1", "recordType": "RecurrenceRule"}]}),
        json!({"records": [{"recordName": "RecurrenceRule/1", "recordType": "RecurrenceRule", "fields": {"List": {"type": "REFERENCE", "value": {"recordName": "List/1"}}}}]}),
        json!({"records": [{"recordName": "RecurrenceRule/1", "recordType": "RecurrenceRule", "fields": {"Reminder": {"type": "REFERENCE", "value": {"recordName": 1}}}}]}),
        json!({"records": [{"recordName": "RecurrenceRule/1", "recordType": "RecurrenceRule", "fields": {"Reminder": {"type": "STRING", "value": "Reminder/12345678"}}}]}),
    ];
    for response in responses {
        let fail_query = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let calls = Calls::default();
        let (log, failing, r) = (calls.clone(), fail_query.clone(), response.clone());
        let c = client(move |path, body| {
            log.push(path, &body);
            Ok(if path == "/records/lookup" {
                json!({"records": [record()]})
            } else if failing.load(Ordering::SeqCst) {
                r.clone()
            } else {
                json!({"records": []})
            })
        });
        let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
        fail_query.store(true, Ordering::SeqCst);
        assert_eq!(
            code(c.delete_reminder(&current).await),
            ErrorCode::Protocol,
            "{response}"
        );
        assert!(!calls.paths().contains(&"/records/modify".to_owned()));
    }
}

#[tokio::test]
async fn ignores_soft_deleted_reminders_with_missing_content_and_list_references() {
    let c = snapshot_client(
        json!([{"recordName": "deleted", "recordType": "Reminder", "fields": {"Deleted": {"type": "INT64", "value": 1}}}]),
    );
    let snapshot = c.read_snapshot().await.unwrap();
    assert_eq!(
        serde_json::to_value(&snapshot).unwrap(),
        json!({
            "lists": [{"id": "List/1", "title": "Groceries", "color": "blue", "count": 0, "recordChangeTag": null}],
            "reminders": [],
        })
    );
}

#[tokio::test]
async fn keeps_encrypted_title_and_notes_documents_unavailable() {
    for field in ["TitleDocument", "NotesDocument"] {
        let c = snapshot_client(json!([record_fields(
            json!({field: {"type": "ENCRYPTED_BYTES", "value": "opaque"}})
        )]));
        assert_eq!(
            code(c.read_snapshot().await),
            ErrorCode::AwaitingDeviceApproval,
            "{field}"
        );
    }
}

#[tokio::test]
async fn decodes_server_decrypted_bytes_and_encrypted_bytes_documents() {
    for ty in ["BYTES", "ENCRYPTED_BYTES"] {
        let mut r = record();
        r["fields"]["TitleDocument"]["type"] = json!(ty);
        let c = snapshot_client(json!([r]));
        assert_eq!(
            c.read_snapshot().await.unwrap().reminders[0].title,
            "Buy milk",
            "{ty}"
        );
    }
}

#[tokio::test]
async fn reads_current_reminders_with_the_bounded_compound_query() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        Ok(if path == "/changes/zone" {
            json!({"zones": [{"records": [list()]}]})
        } else {
            json!({"records": [record()]})
        })
    });
    let snapshot = c.read_snapshot().await.unwrap();
    assert_eq!(
        calls.0.lock().unwrap().clone(),
        vec![
            (
                "/changes/zone".to_owned(),
                json!({"zones": [{"zoneID": {"zoneName": "Reminders", "zoneType": "REGULAR_CUSTOM_ZONE"}, "desiredRecordTypes": ["List"]}]}),
            ),
            (
                "/records/query".to_owned(),
                json!({
                    "zoneID": {"zoneName": "Reminders", "zoneType": "REGULAR_CUSTOM_ZONE"},
                    "resultsLimit": 50,
                    "query": {
                        "recordType": "reminderList",
                        "filterBy": [
                            {"comparator": "EQUALS", "fieldName": "List", "fieldValue": {"type": "REFERENCE", "value": {"recordName": "List/1", "action": "VALIDATE"}}},
                            {"comparator": "EQUALS", "fieldName": "includeCompleted", "fieldValue": {"type": "INT64", "value": 1}},
                            {"comparator": "EQUALS", "fieldName": "LookupValidatingReference", "fieldValue": {"type": "INT64", "value": 1}},
                        ],
                    },
                }),
            ),
        ]
    );
    assert_eq!(
        serde_json::to_value(&snapshot.lists).unwrap(),
        json!([{"id": "List/1", "title": "Groceries", "color": "blue", "count": 1, "recordChangeTag": null}])
    );
    let r = &snapshot.reminders[0];
    assert_eq!(
        (
            r.title.as_str(),
            r.description.as_str(),
            r.list_id.as_str(),
            r.due_date,
            r.priority,
            r.flagged,
            r.all_day
        ),
        (
            "Buy milk",
            "At noon",
            "List/1",
            Some(1_699_920_000_000),
            5,
            true,
            true
        )
    );
}

#[tokio::test]
async fn rejects_a_stalled_cursor_rather_than_returning_a_partial_snapshot() {
    let c = client(|_, _| Ok(json!({"zones": [{"records": [list()], "moreComing": true}]})));
    assert_eq!(code(c.read_snapshot().await), ErrorCode::Protocol);
}

#[tokio::test]
async fn rejects_partial_modify_error_as_conflict_or_protocol() {
    for (server_error, expected) in [
        ("CONFLICT", ErrorCode::Conflict),
        ("UNKNOWN_ERROR", ErrorCode::Protocol),
    ] {
        let calls = Calls::default();
        let log = calls.clone();
        let c = client(move |path, body| {
            log.push(path, &body);
            Ok(match path {
                "/records/lookup" => json!({"records": [record()]}),
                "/records/query" => json!({"records": []}),
                _ => {
                    json!({"records": [{"recordName": "Reminder/12345678", "recordType": "Reminder", "serverErrorCode": server_error}]})
                }
            })
        });
        let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
        assert_eq!(
            code(c.update_reminder(&current, &flag_patch(false)).await),
            expected
        );
        assert_eq!(
            calls.paths(),
            [
                "/records/lookup",
                "/records/query",
                "/records/lookup",
                "/records/query",
                "/records/modify"
            ]
        );
    }
}

#[tokio::test]
async fn checks_the_read_back_value_after_a_tagged_write() {
    let calls = Calls::default();
    let log = calls.clone();
    let lookups = AtomicUsize::new(0);
    let c = client(move |path, body| {
        log.push(path, &body);
        Ok(match path {
            "/records/modify" => {
                json!({"records": [{"recordName": "Reminder/12345678", "recordChangeTag": "tag-2"}]})
            }
            "/records/query" => json!({"records": []}),
            _ => {
                let n = lookups.fetch_add(1, Ordering::SeqCst) + 1;
                json!({"records": [record_with(json!({"recordChangeTag": if n <= 2 { "tag-1" } else { "tag-2" }}))]})
            }
        })
    });
    let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
    assert_eq!(
        code(c.update_reminder(&current, &flag_patch(false)).await),
        ErrorCode::Protocol
    );
    assert_eq!(
        calls.paths(),
        [
            "/records/lookup",
            "/records/query",
            "/records/lookup",
            "/records/query",
            "/records/modify",
            "/records/lookup",
            "/records/query",
        ]
    );
}

#[tokio::test]
async fn rejects_mutations_of_recurring_reminders_before_a_write() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        Ok(
            json!({"records": [record_fields(json!({"RecurrenceRules": {"type": "STRING", "value": "rule"}}))]}),
        )
    });
    let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
    assert!(current.recurring);
    assert_eq!(
        code(c.delete_reminder(&current).await),
        ErrorCode::Unsupported
    );
    assert_eq!(calls.paths(), ["/records/lookup"]);
}

#[tokio::test]
async fn detects_a_separate_recurrence_record_before_a_mutation() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        Ok(if path == "/records/lookup" {
            json!({"records": [record_fields(json!({"RecurrenceRuleIDs": {"type": "UNKNOWN_LIST", "value": []}}))]})
        } else {
            json!({"records": [rule_record("RecurrenceRule/1", "Reminder/12345678")]})
        })
    });
    let current = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
    assert!(current.recurring);
    assert_eq!(
        code(c.set_completed(&current, true).await),
        ErrorCode::Unsupported
    );
    assert_eq!(calls.paths(), ["/records/lookup", "/records/query"]);
}

#[tokio::test]
async fn marks_separately_referenced_recurring_reminders_in_a_snapshot() {
    let c = snapshot_client(json!([
        list(),
        record(),
        rule_record("RecurrenceRule/1", "Reminder/12345678")
    ]));
    assert!(c.read_snapshot().await.unwrap().reminders[0].recurring);
}

#[tokio::test]
async fn rejects_an_existing_create_id_whose_notes_or_dates_differ() {
    let calls = Calls::default();
    let log = calls.clone();
    let c = client(move |path, body| {
        log.push(path, &body);
        Ok(if path == "/records/lookup" {
            json!({"records": [record()]})
        } else {
            json!({"records": []})
        })
    });
    let input = ReminderCreateFields {
        list_id: "List/1".into(),
        title: "Buy milk".into(),
        description: Some("Different notes".into()),
        due_date: Opt::Set(1_699_920_000_000),
        priority: Some(5),
        flagged: Some(true),
        all_day: Some(true),
        ..ReminderCreateFields::default()
    };
    assert_eq!(
        code(c.create_reminder("Reminder/12345678", &input).await),
        ErrorCode::Conflict
    );
    assert_eq!(calls.paths(), ["/records/lookup", "/records/query"]);
}

#[tokio::test]
async fn requires_utc_midnight_for_an_all_day_civil_date() {
    let c = client(|_, _| panic!("network should not be reached"));
    let input = ReminderCreateFields {
        list_id: "List/1".into(),
        title: "Buy milk".into(),
        all_day: Some(true),
        due_date: Opt::Set(1_699_920_000_001),
        ..ReminderCreateFields::default()
    };
    assert_eq!(
        code(c.create_reminder("Reminder/12345678", &input).await),
        ErrorCode::Invalid
    );
}

#[tokio::test]
async fn updates_a_reminder_with_empty_recurrence_ids_and_preserves_unrelated_fields() {
    for recurrence_type in ["UNKNOWN_LIST", "STRING_LIST"] {
        let modified: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let seen = modified.clone();
        let original = record_fields(json!({
            "TimeZone": {"type": "STRING", "value": "America/Vancouver"},
            "RecurrenceRuleIDs": {"type": recurrence_type, "value": []},
        }));
        let c = client(move |path, body| {
            if path == "/records/query" {
                return Ok(json!({"records": []}));
            }
            if path == "/records/modify" {
                *seen.lock().unwrap() = Some(body["operations"][0]["record"]["fields"].clone());
                return Ok(
                    json!({"records": [{"recordName": "Reminder/12345678", "recordChangeTag": "tag-2"}]}),
                );
            }
            let written = seen.lock().unwrap().is_some();
            let mut r = original.clone();
            if written {
                r["recordChangeTag"] = json!("tag-2");
                r["fields"]["Flagged"] = json!({"type": "INT64", "value": 0});
            }
            Ok(json!({"records": [r]}))
        });
        let current: Reminder = c.get_reminder("Reminder/12345678").await.unwrap().unwrap();
        assert!(!current.recurring);
        let updated = c
            .update_reminder(&current, &flag_patch(false))
            .await
            .unwrap();
        assert!(!updated.flagged);
        let fields = modified.lock().unwrap().clone().unwrap();
        assert!(fields.get("Flagged").is_some());
        for absent in ["TimeZone", "DueDate", "RecurrenceRuleIDs"] {
            assert!(fields.get(absent).is_none(), "{absent}");
        }
        assert!(
            fields["ResolutionTokenMap"]["value"]
                .as_str()
                .unwrap()
                .contains("\"flagged\"")
        );
    }
}

#[tokio::test]
async fn round_trips_unicode_and_rejects_malformed_documents() {
    let encoded = encode_crdt_document("Café 🌿\nMilk").unwrap();
    assert_eq!(
        omni_reminders::codec::decode_crdt_document(&encoded).unwrap(),
        "Café 🌿\nMilk"
    );
    assert!(omni_reminders::codec::decode_crdt_document("%%%").is_err());
}
