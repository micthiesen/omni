//! Recurring reminder completion.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use crate::common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_reminders::cloudkit::{CkPost, ErrorCode, Reminder, RemindersError};
use omni_reminders::cloudkit_extras::{ReminderRecurrence, ReminderRecurrences};
use omni_reminders::codec::encode_crdt_document;
use omni_reminders::recurrence::{Frequency, Opt, RecurrenceDecoded, RecurrenceRule};
use omni_reminders::recurring_completion::{
    CompletionDeps, CompletionState, RecurringCompletionInput, RecurringCompletionTarget,
    complete_recurring_occurrence, request_recurring_completion,
};
use serde_json::{Value, json};

fn input() -> RecurringCompletionInput {
    RecurringCompletionInput {
        reminder_id: "Reminder/fixture".into(),
        rule_id: "RecurrenceRule/fixture".into(),
        time_zone: "America/Vancouver".into(),
        owner_record_name: None,
    }
}

fn returned_record() -> Value {
    json!({"recordName": "Reminder/fixture", "recordType": "Reminder", "recordChangeTag": "new"})
}

struct CountingPost {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<(String, Value)>>>,
    respond: Box<dyn Fn() -> Result<Value, RemindersError> + Send + Sync>,
}

impl CkPost for CountingPost {
    fn post(&self, path: &str, body: Value) -> BoxFuture<'static, Result<Value, RemindersError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies.lock().unwrap().push((path.to_owned(), body));
        let result = (self.respond)();
        Box::pin(async move { result })
    }
}

fn counting(
    respond: impl Fn() -> Result<Value, RemindersError> + Send + Sync + 'static,
) -> CountingPost {
    CountingPost {
        calls: Arc::new(AtomicUsize::new(0)),
        bodies: Arc::new(Mutex::new(Vec::new())),
        respond: Box::new(respond),
    }
}

#[tokio::test]
async fn uses_apples_mutating_query_and_returns_unverified_records_for_fresh_verification() {
    let post = counting(|| Ok(json!({"records": [returned_record()]})));
    let response = request_recurring_completion(&post, &input()).await.unwrap();
    assert!(!response.verified);
    assert_eq!(response.records.len(), 1);
    assert_eq!(response.records[0].record_name, "Reminder/fixture");
    assert_eq!(
        response.records[0].record_change_tag.as_deref(),
        Some("new")
    );
    assert_eq!(post.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        post.bodies.lock().unwrap()[0],
        (
            "/records/query".to_owned(),
            json!({
                "zoneID": {"zoneName": "Reminders", "zoneType": "REGULAR_CUSTOM_ZONE"},
                "query": {
                    "recordType": "CompleteRecurringReminder",
                    "filterBy": [
                        {"comparator": "EQUALS", "fieldName": "Reminder", "fieldValue": {"type": "REFERENCE", "value": {"recordName": "Reminder/fixture", "action": "VALIDATE"}}},
                        {"comparator": "EQUALS", "fieldName": "RecurrenceRule", "fieldValue": {"type": "REFERENCE", "value": {"recordName": "RecurrenceRule/fixture", "action": "VALIDATE"}}},
                        {"comparator": "EQUALS", "fieldName": "TimeZone", "fieldValue": {"type": "STRING", "value": "America/Vancouver"}},
                    ],
                },
            })
        )
    );
}

#[tokio::test]
async fn rejects_incomplete_or_malformed_response_without_repeating_the_query() {
    let mut conflicted = returned_record();
    conflicted["serverErrorCode"] = json!("CONFLICT");
    let many: Vec<Value> = (0..101)
        .map(|i| {
            let mut r = returned_record();
            r["recordName"] = json!(format!("Reminder/{i}"));
            r
        })
        .collect();
    for response in [
        json!({"records": []}),
        json!({"records": [returned_record(), returned_record()]}),
        json!({"records": [returned_record()], "continuationMarker": "next"}),
        json!({"records": [conflicted]}),
        json!({"records": many}),
    ] {
        let r = response.clone();
        let post = counting(move || Ok(r.clone()));
        assert!(request_recurring_completion(&post, &input()).await.is_err());
        assert_eq!(post.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn never_retries_a_lost_response() {
    let post = counting(|| {
        Err(RemindersError {
            operation: "fixture".into(),
            code: ErrorCode::Transport,
        })
    });
    assert!(request_recurring_completion(&post, &input()).await.is_err());
    assert_eq!(post.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn validates_identities_and_explicit_timezone_before_mutation() {
    for patch in [
        RecurringCompletionInput {
            time_zone: "invalid zone".into(),
            ..input()
        },
        RecurringCompletionInput {
            reminder_id: "List/fixture".into(),
            ..input()
        },
        RecurringCompletionInput {
            rule_id: "Reminder/fixture".into(),
            ..input()
        },
    ] {
        let post = counting(|| Ok(json!({"records": [returned_record()]})));
        assert!(request_recurring_completion(&post, &patch).await.is_err());
        assert_eq!(post.calls.load(Ordering::SeqCst), 0);
    }
}

/// The zones Node 24's `Intl.DateTimeFormat` accepts (IANA names in any
/// case, links, and `±HH`, `±HHMM`, `±HH:MM` offsets) reach Apple unchanged; the
/// forms it rejects never issue the mutating query.
#[tokio::test]
async fn accepts_exactly_the_time_zones_intl_accepts() {
    for zone in [
        "america/vancouver",
        "US/Pacific",
        "utc",
        "Etc/GMT+5",
        "+23:59",
        "-00",
        "+0530",
        "\u{2212}05:00",
    ] {
        let post = counting(|| Ok(json!({"records": [returned_record()]})));
        let request = RecurringCompletionInput {
            time_zone: zone.into(),
            ..input()
        };
        assert!(
            request_recurring_completion(&post, &request).await.is_ok(),
            "{zone}"
        );
        assert_eq!(post.calls.load(Ordering::SeqCst), 1, "{zone}");
    }
    for zone in [
        "+24:00",
        "+5",
        "+05:0",
        "+0500:",
        "+05:00:00",
        "+05:60",
        "Etc/Unknown",
        "+1\u{e9}2",
        "America/Vancouver ",
    ] {
        let post = counting(|| Ok(json!({"records": [returned_record()]})));
        let request = RecurringCompletionInput {
            time_zone: zone.into(),
            ..input()
        };
        assert!(
            request_recurring_completion(&post, &request).await.is_err(),
            "{zone}"
        );
        assert_eq!(post.calls.load(Ordering::SeqCst), 0, "{zone}");
    }
}

// ---- verified recurring occurrence advancement ----

const DUE: i64 = 1_793_466_000_000;
const RULE: &str = "RecurrenceRule/fixture";

fn daily_rule() -> RecurrenceRule {
    RecurrenceRule {
        frequency: Frequency::Daily,
        interval: 1,
        occurrence_count: Opt::Absent,
        end_date: Opt::Absent,
        first_day_of_week: Opt::Set(0),
        days_of_week: Opt::Absent,
        days_of_month: Opt::Absent,
        days_of_year: Opt::Absent,
        weeks_of_year: Opt::Absent,
        months_of_year: Opt::Absent,
        set_positions: Opt::Absent,
    }
}

fn recurrences(tag: &str, rule: RecurrenceRule) -> ReminderRecurrences {
    ReminderRecurrences {
        reminder_id: "Reminder/fixture".into(),
        reminder_change_tag: tag.into(),
        rules: vec![ReminderRecurrence {
            id: RULE.into(),
            reminder_id: "Reminder/fixture".into(),
            record_change_tag: Some("rule-tag".into()),
            writable: true,
            recurrence: RecurrenceDecoded::Supported(rule),
        }],
    }
}

/// The spec's `completionFixture()`, as mutable state behind the deps seam.
struct Fixture {
    clock: SharedClock,
    before: Mutex<Reminder>,
    current: Mutex<Reminder>,
    completed: Mutex<Reminder>,
    after_rule: Mutex<ReminderRecurrences>,
    related: Mutex<ReminderRecurrences>,
    returned: Mutex<Vec<Value>>,
    called: AtomicBool,
    posts: AtomicUsize,
    clone_reads: AtomicUsize,
    clone_raced: AtomicBool,
}

impl Fixture {
    fn new() -> Arc<Self> {
        let clock: SharedClock = omni_testkit::test_clock(common::NOW);
        let before = Reminder {
            id: "Reminder/fixture".into(),
            list_id: "List/fixture".into(),
            title: "Synthetic".into(),
            description: "Own test only".into(),
            completed: false,
            completed_date: None,
            due_date: Some(DUE),
            start_date: None,
            priority: 5,
            flagged: true,
            all_day: false,
            deleted: false,
            created_date: Some(1),
            last_modified_date: Some(2),
            record_change_tag: "before".into(),
            recurring: true,
        };
        let current = Reminder {
            due_date: Some(DUE + 25 * 3_600_000),
            record_change_tag: "after".into(),
            ..before.clone()
        };
        let completed = Reminder {
            id: "Reminder/completed-copy".into(),
            completed: true,
            completed_date: Some(clock.now_ms()),
            record_change_tag: "copy".into(),
            recurring: false,
            ..before.clone()
        };
        let x = Arc::new(Self {
            clock,
            before: Mutex::new(before),
            current: Mutex::new(current),
            completed: Mutex::new(completed),
            after_rule: Mutex::new(recurrences("after", daily_rule())),
            related: Mutex::new(recurrences("before", daily_rule())),
            returned: Mutex::new(Vec::new()),
            called: AtomicBool::new(false),
            posts: AtomicUsize::new(0),
            clone_reads: AtomicUsize::new(0),
            clone_raced: AtomicBool::new(false),
        });
        x.refresh_previews();
        x
    }

    fn preview(reminder: &Reminder) -> Value {
        let mut fields = json!({
            "Completed": {"type": "INT64", "value": i64::from(reminder.completed)},
            "DueDate": {"type": "TIMESTAMP", "value": reminder.due_date},
        });
        if let Some(date) = reminder.completed_date {
            fields["CompletionDate"] = json!({"type": "TIMESTAMP", "value": date});
        }
        fields
    }

    fn refresh_previews(&self) {
        let before_tag = self.before.lock().unwrap().record_change_tag.clone();
        let current = self.current.lock().unwrap().clone();
        let completed = self.completed.lock().unwrap().clone();
        *self.returned.lock().unwrap() = vec![
            json!({"recordName": current.id, "recordType": "Reminder", "recordChangeTag": before_tag, "fields": Self::preview(&current)}),
            json!({"recordName": completed.id, "recordType": "Reminder", "recordChangeTag": before_tag, "fields": Self::preview(&completed)}),
            json!({"recordName": "List/fixture", "recordType": "List", "recordChangeTag": "list"}),
        ];
    }

    fn target(&self) -> RecurringCompletionTarget {
        RecurringCompletionTarget {
            id: "Reminder/fixture".into(),
            change_tag: "before".into(),
            rule_id: RULE.into(),
            rule_change_tag: "rule-tag".into(),
            time_zone: "America/Los_Angeles".into(),
        }
    }

    fn set_rule(&self, rule: RecurrenceRule) {
        self.related.lock().unwrap().rules[0].recurrence =
            RecurrenceDecoded::Supported(rule.clone());
        self.after_rule.lock().unwrap().rules[0].recurrence = RecurrenceDecoded::Supported(rule);
    }

    /// The "ended" shape: no clone, the root completes on the same due date.
    fn ended(&self, rule: RecurrenceRule) {
        self.returned.lock().unwrap().remove(1);
        {
            let mut current = self.current.lock().unwrap();
            current.due_date = self.before.lock().unwrap().due_date;
            current.completed = true;
            current.completed_date = Some(self.clock.now_ms());
        }
        self.set_rule(rule);
        let current = self.current.lock().unwrap().clone();
        self.returned.lock().unwrap()[0]["fields"] = Self::preview(&current);
    }
}

struct Deps(Arc<Fixture>);

impl CkPost for Deps {
    fn post(&self, _path: &str, _body: Value) -> BoxFuture<'static, Result<Value, RemindersError>> {
        self.0.called.store(true, Ordering::SeqCst);
        self.0.posts.fetch_add(1, Ordering::SeqCst);
        let records = self.0.returned.lock().unwrap().clone();
        Box::pin(async move { Ok(json!({"records": records})) })
    }
}

impl CompletionDeps for Deps {
    fn post(&self) -> &dyn CkPost {
        self
    }

    fn clock(&self) -> &SharedClock {
        &self.0.clock
    }

    fn get_reminder<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Reminder>, RemindersError>> {
        let x = &self.0;
        let called = x.called.load(Ordering::SeqCst);
        let result = if id == x.before.lock().unwrap().id {
            Some(if called {
                x.current.lock().unwrap().clone()
            } else {
                x.before.lock().unwrap().clone()
            })
        } else if id == x.completed.lock().unwrap().id {
            let reads = x.clone_reads.fetch_add(1, Ordering::SeqCst) + 1;
            let mut copy = x.completed.lock().unwrap().clone();
            if x.clone_raced.load(Ordering::SeqCst) && reads > 1 {
                copy.record_change_tag = "raced".into();
            }
            Some(copy)
        } else {
            None
        };
        Box::pin(async move { Ok(result) })
    }

    fn get_recurrences<'a>(
        &'a self,
        _id: &'a str,
    ) -> BoxFuture<'a, Result<ReminderRecurrences, RemindersError>> {
        let x = &self.0;
        let result = if x.called.load(Ordering::SeqCst) {
            x.after_rule.lock().unwrap().clone()
        } else {
            x.related.lock().unwrap().clone()
        };
        Box::pin(async move { Ok(result) })
    }
}

async fn run(
    x: &Arc<Fixture>,
    target: RecurringCompletionTarget,
) -> Result<omni_reminders::recurring_completion::VerifiedRecurringCompletion, RemindersError> {
    complete_recurring_occurrence(&Deps(x.clone()), &target).await
}

fn utc(y: i16, m: i8, d: i8, h: i8) -> i64 {
    jiff::civil::date(y, m, d)
        .at(h, 0, 0, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond()
}

#[tokio::test]
async fn rejects_apples_obsolete_vancouver_fall_back_adjustment_without_repairing_or_repeating_it()
{
    let x = Fixture::new();
    // tzdata 2026b+: Vancouver stays on UTC-7, so a 25-hour advance moves 10am to 11am.
    let target = RecurringCompletionTarget {
        time_zone: "America/Vancouver".into(),
        ..x.target()
    };
    assert_eq!(
        run(&x, target).await.unwrap_err().code,
        ErrorCode::Uncertain
    );
    assert_eq!(x.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accepts_advancement_preserving_local_time_over_dst_specific_intervals() {
    for (zone, due, hours) in [
        ("America/Vancouver", DUE, 24),
        ("America/Los_Angeles", utc(2026, 3, 7, 18), 23),
    ] {
        let x = Fixture::new();
        x.before.lock().unwrap().due_date = Some(due);
        x.current.lock().unwrap().due_date = Some(due + hours * 3_600_000);
        x.completed.lock().unwrap().due_date = Some(due);
        x.refresh_previews();
        let target = RecurringCompletionTarget {
            time_zone: zone.into(),
            ..x.target()
        };
        let receipt = run(&x, target).await.unwrap();
        assert_eq!(
            (receipt.state, receipt.verified),
            (CompletionState::Advanced, true),
            "{zone}"
        );
    }
}

#[tokio::test]
async fn leaves_all_day_civil_date_verification_separate_from_timed_local_clock_checks() {
    let x = Fixture::new();
    let due = utc(2026, 11, 1, 0);
    {
        let mut before = x.before.lock().unwrap();
        before.due_date = Some(due);
        before.all_day = true;
    }
    {
        let mut current = x.current.lock().unwrap();
        current.due_date = Some(due + 86_400_000);
        current.all_day = true;
    }
    {
        let mut completed = x.completed.lock().unwrap();
        completed.due_date = Some(due);
        completed.all_day = true;
    }
    x.refresh_previews();
    let receipt = run(&x, x.target()).await.unwrap();
    assert_eq!(receipt.state, CompletionState::Advanced);
}

#[tokio::test]
async fn keeps_preview_mismatches_uncertain_without_trusting_preview_change_tags() {
    for kind in [
        "wrong-completed-type",
        "wrong-due-preview",
        "missing-copy-completion",
        "unchanged-root-tag",
        "clone-raced",
    ] {
        let x = Fixture::new();
        match kind {
            "wrong-completed-type" => {
                x.returned.lock().unwrap()[0]["fields"]["Completed"]["type"] = json!("STRING")
            }
            "wrong-due-preview" => {
                x.returned.lock().unwrap()[0]["fields"]["DueDate"]["value"] = json!(1)
            }
            "missing-copy-completion" => {
                x.returned.lock().unwrap()[1]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("CompletionDate");
            }
            "unchanged-root-tag" => {
                x.current.lock().unwrap().record_change_tag = "before".into();
                x.after_rule.lock().unwrap().reminder_change_tag = "before".into();
            }
            _ => x.clone_raced.store(true, Ordering::SeqCst),
        }
        assert_eq!(
            run(&x, x.target()).await.unwrap_err().code,
            ErrorCode::Uncertain,
            "{kind}"
        );
        assert_eq!(x.posts.load(Ordering::SeqCst), 1, "{kind}");
    }
}

#[tokio::test]
async fn retains_the_indexed_snapshot_and_discovers_the_completed_clone_through_incremental_refresh()
 {
    let int = |v: i64| json!({"type": "INT64", "value": v});
    let stamp = |v: i64| json!({"type": "TIMESTAMP", "value": v});
    let reference =
        |n: &str| json!({"type": "REFERENCE", "value": {"recordName": n, "action": "VALIDATE"}});
    let list = json!({"recordName": "List/fixture", "recordType": "List", "fields": {"Name": {"type": "STRING", "value": "Synthetic"}}});
    let original = json!({
        "recordName": "Reminder/fixture",
        "recordType": "Reminder",
        "recordChangeTag": "before",
        "fields": {
            "TitleDocument": {"type": "STRING", "value": encode_crdt_document("Own synthetic reminder").unwrap()},
            "List": reference("List/fixture"),
            "Completed": int(0),
            "DueDate": stamp(DUE),
            "RecurrenceRuleIDs": {"type": "STRING_LIST", "value": ["ABCDEFGH"]},
        },
    });
    let rule_id = "RecurrenceRule/ABCDEFGH";
    let rule = json!({
        "recordName": rule_id,
        "recordType": "RecurrenceRule",
        "recordChangeTag": "rule-tag",
        "fields": {"Reminder": reference("Reminder/fixture"), "Frequency": int(0), "Interval": int(1)},
    });
    let mut root = original.clone();
    root["recordChangeTag"] = json!("advanced");
    root["fields"]["DueDate"] = stamp(DUE + 25 * 3_600_000);
    let mut clone = original.clone();
    clone["recordName"] = json!("Reminder/completed-copy");
    clone["recordChangeTag"] = json!("clone");
    clone["fields"]["Completed"] = int(1);
    clone["fields"]["CompletionDate"] = stamp(common::NOW);
    clone["fields"]["RecurrenceRuleIDs"] = json!({"type": "STRING_LIST", "value": []});
    let mutated = Arc::new(AtomicBool::new(false));
    let mutations = Arc::new(AtomicUsize::new(0));
    let zone_calls = Arc::new(AtomicUsize::new(0));
    let (m, mc, zc) = (mutated.clone(), mutations.clone(), zone_calls.clone());
    let c = common::client(move |path, body| {
        Ok(if path == "/changes/zone" {
            let n = zc.fetch_add(1, Ordering::SeqCst) + 1;
            let records = if n == 1 {
                json!([list])
            } else if m.load(Ordering::SeqCst) {
                json!([root, clone])
            } else {
                json!([])
            };
            json!({"zones": [{"records": records, "syncToken": format!("token-{n}")}]})
        } else if path == "/records/lookup" {
            let id = body["records"][0]["recordName"].as_str().unwrap();
            let record = if id == rule_id {
                rule.clone()
            } else if id == "Reminder/completed-copy" {
                clone.clone()
            } else if m.load(Ordering::SeqCst) {
                root.clone()
            } else {
                original.clone()
            };
            json!({"records": [record]})
        } else if body["query"]["recordType"] == "CompleteRecurringReminder" {
            m.store(true, Ordering::SeqCst);
            mc.fetch_add(1, Ordering::SeqCst);
            let mut root_preview = root.clone();
            root_preview["recordChangeTag"] = json!("before");
            let mut clone_preview = clone.clone();
            clone_preview["recordChangeTag"] = json!("before");
            json!({"records": [root_preview, clone_preview, list]})
        } else if m.load(Ordering::SeqCst) {
            json!({"records": [root, clone, rule]})
        } else {
            json!({"records": [original, rule]})
        })
    });
    c.read_snapshot().await.unwrap();
    assert!(c.has_snapshot());
    let receipt = c
        .complete_recurring(&RecurringCompletionTarget {
            id: "Reminder/fixture".into(),
            change_tag: "before".into(),
            rule_id: rule_id.into(),
            rule_change_tag: "rule-tag".into(),
            time_zone: "America/Los_Angeles".into(),
        })
        .await
        .unwrap();
    assert_eq!(receipt.state, CompletionState::Advanced);
    assert_eq!(receipt.reminder_change_tag, "advanced");
    assert_eq!(receipt.completed_reminder_change_tag, "clone");
    assert!(c.has_snapshot());
    assert!(zone_calls.load(Ordering::SeqCst) > 1);
    assert_eq!(mutations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn keeps_outcome_uncertain_when_fresh_rule_tag_or_writable_changes_without_decoded_value_changes()
 {
    for (state, field) in [
        ("advanced", "tag"),
        ("ended", "tag"),
        ("advanced", "writable"),
        ("ended", "writable"),
    ] {
        let x = Fixture::new();
        if state == "ended" {
            x.ended(RecurrenceRule {
                end_date: Opt::Set(DUE + 60_000),
                first_day_of_week: Opt::Absent,
                ..daily_rule()
            });
        }
        {
            let mut after = x.after_rule.lock().unwrap();
            if field == "tag" {
                after.rules[0].record_change_tag = Some("concurrently-changed".into());
            } else {
                after.rules[0].writable = false;
            }
        }
        assert_eq!(
            run(&x, x.target()).await.unwrap_err().code,
            ErrorCode::Uncertain,
            "{state} {field}"
        );
        assert_eq!(x.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn verifies_a_simple_finite_daily_series_ending_on_this_occurrences_civil_date() {
    let x = Fixture::new();
    x.ended(RecurrenceRule {
        end_date: Opt::Set(DUE + 60_000),
        first_day_of_week: Opt::Absent,
        ..daily_rule()
    });
    let receipt = run(&x, x.target()).await.unwrap();
    assert_eq!(receipt.state, CompletionState::Ended);
    assert!(receipt.verified);
    assert_eq!(receipt.next_due_date, None);
    assert_eq!(receipt.completed_reminder_id, "Reminder/fixture");
}

#[tokio::test]
async fn does_not_infer_series_exhaustion_for_unproven_shapes() {
    for kind in ["later-end-date", "count-only", "hourly", "selectors"] {
        let x = Fixture::new();
        let mut rule = RecurrenceRule {
            end_date: Opt::Set(DUE + 60_000),
            first_day_of_week: Opt::Absent,
            ..daily_rule()
        };
        match kind {
            "later-end-date" => rule.end_date = Opt::Set(DUE + 7 * 86_400_000),
            "count-only" => {
                rule.end_date = Opt::Absent;
                rule.occurrence_count = Opt::Set(1);
            }
            "hourly" => rule.frequency = Frequency::Hourly,
            _ => rule.days_of_month = Opt::Set(vec![1]),
        }
        x.ended(rule);
        assert_eq!(
            run(&x, x.target()).await.unwrap_err().code,
            ErrorCode::Uncertain,
            "{kind}"
        );
        assert_eq!(x.posts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn verifies_same_identity_advancement_and_completed_copy_across_the_25_hour_los_angeles_dst_transition()
 {
    let x = Fixture::new();
    let receipt = run(&x, x.target()).await.unwrap();
    assert_eq!(receipt.state, CompletionState::Advanced);
    assert!(receipt.verified);
    assert_eq!(receipt.reminder_id, "Reminder/fixture");
    assert_eq!(receipt.completed_reminder_id, "Reminder/completed-copy");
    assert_eq!(receipt.previous_due_date, DUE);
    assert_eq!(receipt.next_due_date, Some(DUE + 25 * 3_600_000));
    assert_eq!(x.posts.load(Ordering::SeqCst), 1);
    let returned = x.returned.lock().unwrap();
    assert_eq!(returned[0]["recordChangeTag"], "before");
    assert_eq!(returned[1]["recordChangeTag"], "before");
}

#[tokio::test]
async fn rejects_preflight_failures_before_querying() {
    for kind in [
        "stale-reminder",
        "stale-rule",
        "already-completed",
        "unknown-rule",
        "no-due",
        "invalid-zone",
    ] {
        let x = Fixture::new();
        let mut target = x.target();
        match kind {
            "stale-reminder" => target.change_tag = "wrong".into(),
            "stale-rule" => target.rule_change_tag = "wrong".into(),
            "already-completed" => x.before.lock().unwrap().completed = true,
            "unknown-rule" => x.related.lock().unwrap().rules[0].writable = false,
            "no-due" => x.before.lock().unwrap().due_date = None,
            _ => target.time_zone = "not a zone".into(),
        }
        assert!(run(&x, target).await.is_err(), "{kind}");
        assert_eq!(x.posts.load(Ordering::SeqCst), 0, "{kind}");
    }
}

#[tokio::test]
async fn keeps_unexpected_outcomes_uncertain_after_exactly_one_query() {
    let kinds = [
        "missing-copy",
        "unchanged-due",
        "root-completed",
        "changed-content",
        "changed-list",
        "copy-incomplete",
        "copy-recurring",
        "old-completion",
        "wrong-copy-due",
        "tag-raced",
        "rule-changed",
    ];
    for kind in kinds {
        let x = Fixture::new();
        match kind {
            "missing-copy" => {
                x.returned.lock().unwrap().remove(1);
            }
            "unchanged-due" => x.current.lock().unwrap().due_date = Some(DUE),
            "root-completed" => x.current.lock().unwrap().completed = true,
            "changed-content" => x.current.lock().unwrap().description = "Unexpected edit".into(),
            "changed-list" => x.completed.lock().unwrap().list_id = "List/other".into(),
            "copy-incomplete" => x.completed.lock().unwrap().completed = false,
            "copy-recurring" => x.completed.lock().unwrap().recurring = true,
            "old-completion" => x.completed.lock().unwrap().completed_date = Some(1),
            "wrong-copy-due" => x.completed.lock().unwrap().due_date = Some(1),
            "tag-raced" => x.current.lock().unwrap().record_change_tag = "concurrent".into(),
            _ => {
                x.after_rule.lock().unwrap().rules[0].recurrence =
                    RecurrenceDecoded::Supported(RecurrenceRule {
                        frequency: Frequency::Weekly,
                        first_day_of_week: Opt::Absent,
                        ..daily_rule()
                    });
            }
        }
        assert_eq!(
            run(&x, x.target()).await.unwrap_err().code,
            ErrorCode::Uncertain,
            "{kind}"
        );
        assert_eq!(x.posts.load(Ordering::SeqCst), 1, "{kind}");
    }
}

/// The completion record does not declare `created`, `modified` or `reason`,
/// so unexpected shapes there do not make a preview a protocol failure.
#[tokio::test]
async fn ignores_undeclared_preview_metadata() {
    let post = counting(|| {
        let mut record = returned_record();
        record["created"] = json!("opaque");
        record["modified"] = json!(5);
        record["reason"] = json!({"detail": 1});
        Ok(json!({"records": [record]}))
    });
    let response = request_recurring_completion(&post, &input()).await.unwrap();
    assert_eq!(response.records.len(), 1);
    assert_eq!(post.calls.load(Ordering::SeqCst), 1);
}
