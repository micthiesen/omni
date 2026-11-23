//! The primary iCloud calendar against a fake CalDAV server: identity
//! pinning, sync-collection and its fallbacks, the change feed, recurrence
//! expansion, and idempotent verified writes through the MCP tools.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::fake_caldav;

use std::sync::Arc;

use fake_caldav::{FakeCaldav, OTHER, PRIMARY};
use omni_calendar::caldav::CaldavSettings;
use omni_calendar::mcp::{CalendarTools, calendar_tools};
use omni_calendar::primary::store::{
    OperationRecord, OperationState, OperationStep, PrimaryPin, StepKind, StepState,
};
use omni_calendar::primary::{PrimaryCalendar, PrimaryDeps};
use omni_core::clock::TestClock;
use omni_http::SideEffectMode;
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput};
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::TestApp;
use serde_json::{Value, json};

/// 2026-10-01T16:00:00Z.
const NOW_MS: i64 = 1_790_870_400_000;

struct Harness {
    app: TestApp,
    fake: FakeCaldav,
    clock: Arc<TestClock>,
    service: PrimaryCalendar,
    tools: Vec<McpTool>,
}

async fn harness() -> Harness {
    let app = TestApp::new().await;
    let fake = FakeCaldav::start().await;
    let clock = omni_testkit::test_clock(NOW_MS);
    let service = PrimaryCalendar::new(PrimaryDeps {
        http: fake.http(),
        settings: Some(CaldavSettings {
            username: "user@icloud.com".to_owned(),
            password: "app-password".to_owned(),
            calendar_url: None,
            calendar_name: None,
        }),
        store: app.ctx.store.clone(),
        clock: clock.clone(),
        mode: SideEffectMode::Live,
        recorded: Arc::default(),
        default_tz: "America/Vancouver".to_owned(),
        tracker: None,
        ports: app.ctx.ports.clone(),
    });
    let tools = calendar_tools(CalendarTools {
        store: app.ctx.store.clone(),
        primary: service.clone(),
    })
    .unwrap();
    Harness {
        app,
        fake,
        clock,
        service,
        tools,
    }
}

impl Harness {
    async fn call(&self, name: &str, input: Value) -> Result<Value, String> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        match tool.handler.call(input, standalone_context("test")).await {
            Ok(ToolOutput::Structured(map)) => Ok(Value::Object(map)),
            Ok(ToolOutput::Custom { structured, .. }) => Ok(Value::Object(structured)),
            Err(e) => Err(e.message),
        }
    }

    async fn ok(&self, name: &str, input: Value) -> Value {
        match self.call(name, input).await {
            Ok(v) => v,
            Err(e) => panic!("{name} failed: {e}"),
        }
    }

    /// Lets the mirror go stale so the next read syncs.
    fn later(&self, ms: i64) {
        self.clock.set(self.clock.now_ms_value() + ms);
    }

    async fn list(&self, from: &str, to: &str) -> Vec<Value> {
        self.ok(
            "calendar_events_list",
            json!({ "from": from, "to": to, "fresh": true }),
        )
        .await["events"]
            .as_array()
            .unwrap()
            .clone()
    }
}

trait ClockExt {
    fn now_ms_value(&self) -> i64;
}

impl ClockExt for TestClock {
    fn now_ms_value(&self) -> i64 {
        omni_core::clock::Clock::now_ms(self)
    }
}

fn calendar(events: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Apple Inc.//iOS 18//EN\r\n{events}END:VCALENDAR\r\n"
    )
}

fn vevent(lines: &[&str]) -> String {
    let mut out = String::from("BEGIN:VEVENT\r\nDTSTAMP:20260901T000000Z\r\n");
    for line in lines {
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("END:VEVENT\r\n");
    out
}

fn weekly() -> String {
    calendar(&format!(
        "{}{}",
        vevent(&[
            "UID:weekly-1",
            "SUMMARY:Standup",
            "DTSTART;TZID=America/Vancouver:20261005T090000",
            "DTEND;TZID=America/Vancouver:20261005T100000",
            "RRULE:FREQ=WEEKLY;COUNT=6",
            "EXDATE;TZID=America/Vancouver:20261012T090000",
            "SEQUENCE:0",
        ]),
        vevent(&[
            "UID:weekly-1",
            "RECURRENCE-ID;TZID=America/Vancouver:20261019T090000",
            "SUMMARY:Standup (moved)",
            "DTSTART;TZID=America/Vancouver:20261019T110000",
            "DTEND;TZID=America/Vancouver:20261019T120000",
            "SEQUENCE:0",
        ])
    ))
}

fn trip() -> String {
    calendar(&vevent(&[
        "UID:trip-1",
        "SUMMARY:Tofino trip",
        "DTSTART;VALUE=DATE:20261010",
        "DTEND;VALUE=DATE:20261013",
    ]))
}

fn dentist() -> String {
    calendar(&vevent(&[
        "UID:dentist-1",
        "SUMMARY:Dentist",
        "LOCATION:Clinic",
        "DTSTART;TZID=America/Vancouver:20261008T140000",
        "DTEND;TZID=America/Vancouver:20261008T150000",
    ]))
}

fn titles(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            format!(
                "{} {}",
                e["start"].as_str().unwrap(),
                e["title"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test]
async fn registers_the_calendar_tools_in_serving_order() {
    let h = harness().await;
    let names: Vec<&str> = h.tools.iter().map(|t| t.meta.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "calendar_status",
            "calendar_events_list",
            "calendar_events_search",
            "calendar_event_get",
            "calendar_event_preview",
            "calendar_write_status",
            "calendar_changes_list",
            "calendar_tracked_events_list",
            "calendar_event_create",
            "calendar_event_update",
            "calendar_event_delete",
        ]
    );
}

#[tokio::test]
async fn pins_the_one_icloud_event_calendar_that_is_the_server_default() {
    let h = harness().await;
    let status = h.ok("calendar_status", json!({})).await;
    assert_eq!(status["state"], "ready", "{status}");
    assert_eq!(status["calendarName"], "iCloud");
    assert_eq!(status["isServerDefault"], true);
    assert_eq!(status["writable"], true);
    assert_eq!(status["supportsSync"], true);
    let text = status.to_string();
    assert!(!text.contains("/calendars/"), "no URLs leak: {text}");
}

#[tokio::test]
async fn fails_closed_when_the_identity_is_ambiguous_and_repins_when_it_is_not() {
    let h = harness().await;
    h.ok("calendar_status", json!({})).await;

    // Two event calendars named "iCloud".
    h.fake.lock().collections[1].name = "iCloud".to_owned();
    h.later(7 * 60 * 60 * 1000);
    let status = h.ok("calendar_status", json!({})).await;
    assert_eq!(status["state"], "identity_error", "{status}");
    assert_eq!(status["errorCode"], "calendar_identity_ambiguous");
    let err = h
        .call("calendar_events_list", json!({ "fresh": true }))
        .await
        .unwrap_err();
    assert!(err.contains("calendar_identity_ambiguous"), "{err}");

    // One "iCloud" calendar again, but the server default names another.
    h.fake.lock().collections[1].name = "Home".to_owned();
    h.fake.lock().default_calendar = Some(OTHER.to_owned());
    h.service.invalidate_identity();
    let status = h.ok("calendar_status", json!({})).await;
    assert_eq!(
        status["errorCode"], "calendar_identity_mismatch",
        "{status}"
    );

    // An unambiguous different calendar re-pins automatically.
    {
        let mut s = h.fake.lock();
        s.collections[0].name = "Work".to_owned();
        s.collections[1].name = "iCloud".to_owned();
    }
    h.service.invalidate_identity();
    let status = h.ok("calendar_status", json!({})).await;
    assert_eq!(status["state"], "ready", "{status}");
    let pin = h
        .app
        .ctx
        .store
        .read(|docs| docs.get::<PrimaryPin>(&"primary".to_owned()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pin.collection_segment, "home");
    assert!(pin.repinned_at.is_some());
    assert!(pin.previous_path_sha256.is_some());
}

#[tokio::test]
async fn records_a_baseline_then_a_change_feed_of_external_edits() {
    let h = harness().await;
    h.fake.put("dentist-1.ics", &dentist());
    h.fake.put("trip-1.ics", &trip());
    let events = h.list("2026-10-01", "2026-10-31").await;
    assert_eq!(events.len(), 2);
    let feed = h.ok("calendar_changes_list", json!({})).await;
    assert_eq!(feed["changes"], json!([]), "the first sync is a baseline");
    let cursor = feed["nextCursor"].as_str().unwrap().to_owned();

    h.fake.put(
        "dentist-1.ics",
        &dentist().replace("Dentist", "Dentist (cleaning)"),
    );
    h.fake.remove("trip-1.ics");
    h.fake.put(
        "new-1.ics",
        &calendar(&vevent(&[
            "UID:new-1",
            "SUMMARY:Lunch",
            "DTSTART;TZID=America/Vancouver:20261009T120000",
            "DTEND;TZID=America/Vancouver:20261009T130000",
        ])),
    );
    h.later(60_000);
    h.list("2026-10-01", "2026-10-31").await;

    let feed = h
        .ok("calendar_changes_list", json!({ "cursor": cursor }))
        .await;
    let changes = feed["changes"].as_array().unwrap();
    let kinds: Vec<(String, String)> = changes
        .iter()
        .map(|c| {
            (
                c["eventId"].as_str().unwrap().to_owned(),
                c["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(
        kinds.contains(&("dentist-1.ics".to_owned(), "updated".to_owned())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("trip-1.ics".to_owned(), "deleted".to_owned())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("new-1.ics".to_owned(), "created".to_owned())),
        "{kinds:?}"
    );
    let updated = changes.iter().find(|c| c["kind"] == "updated").unwrap();
    assert_eq!(updated["changedFields"], json!(["title"]));
    assert_eq!(updated["origin"], "external");
    assert_eq!(updated["before"]["title"], "Dentist");
    assert_eq!(updated["after"]["title"], "Dentist (cleaning)");

    let next = feed["nextCursor"].as_str().unwrap();
    let empty = h
        .ok("calendar_changes_list", json!({ "cursor": next }))
        .await;
    assert_eq!(empty["changes"], json!([]));
    assert_eq!(empty["nextCursor"], next);
}

#[tokio::test]
async fn falls_back_to_a_full_listing_when_the_sync_token_is_rejected() {
    let h = harness().await;
    h.fake.put("dentist-1.ics", &dentist());
    h.list("2026-10-01", "2026-10-31").await;
    h.fake.lock().reject_tokens = true;
    h.fake
        .put("dentist-1.ics", &dentist().replace("Clinic", "New clinic"));
    h.fake.clear_requests();
    h.later(60_000);
    let events = h.list("2026-10-01", "2026-10-31").await;
    assert_eq!(events[0]["location"], "New clinic");
    let requests = h.fake.lock().requests.clone();
    assert!(
        requests
            .iter()
            .any(|r| r.method == "PROPFIND" && r.path == PRIMARY),
        "full ETag listing after the 403 valid-sync-token"
    );
    let feed = h.ok("calendar_changes_list", json!({})).await;
    assert_eq!(feed["changes"][0]["changedFields"], json!(["location"]));
}

#[tokio::test]
async fn follows_truncated_sync_results() {
    let h = harness().await;
    h.list("2026-10-01", "2026-10-31").await;
    h.fake.lock().page_size = Some(1);
    h.fake.put("dentist-1.ics", &dentist());
    h.fake.put("trip-1.ics", &trip());
    h.fake.put("weekly-1.ics", &weekly());
    h.later(60_000);
    h.list("2026-10-01", "2026-10-31").await;
    let feed = h.ok("calendar_changes_list", json!({})).await;
    assert_eq!(feed["changes"].as_array().unwrap().len(), 3, "{feed}");
}

#[tokio::test]
async fn expands_series_with_exdates_overrides_and_multi_day_all_day_events() {
    let h = harness().await;
    h.fake.put("weekly-1.ics", &weekly());
    h.fake.put("trip-1.ics", &trip());
    let events = h.list("2026-10-01", "2026-11-15").await;
    assert_eq!(
        titles(&events),
        [
            "2026-10-05T09:00:00 Standup",
            "2026-10-10 Tofino trip",
            "2026-10-19T11:00:00 Standup (moved)",
            "2026-10-26T09:00:00 Standup",
            "2026-11-02T09:00:00 Standup",
            "2026-11-09T09:00:00 Standup",
        ]
    );
    let trip = &events[1];
    assert_eq!(trip["allDay"], true);
    assert_eq!(trip["lastDate"], "2026-10-12");
    assert_eq!(trip["end"], "2026-10-13");
    let moved = &events[2];
    assert_eq!(moved["isException"], true);
    assert_eq!(
        moved["recurrenceId"],
        "2026-10-19T09:00:00[America/Vancouver]"
    );
    assert_eq!(events[0]["startUtc"], "2026-10-05T16:00:00Z");

    let detail = h
        .ok("calendar_event_get", json!({ "eventId": "weekly-1.ics" }))
        .await;
    assert_eq!(detail["recurrence"]["frequency"], "weekly");
    assert_eq!(detail["recurrence"]["count"], 6);
    assert_eq!(
        detail["exceptions"]["deleted"],
        json!(["2026-10-12T09:00:00[America/Vancouver]"])
    );
    assert_eq!(
        detail["exceptions"]["changed"][0]["title"],
        "Standup (moved)"
    );
    assert_eq!(detail["writable"], true);

    let found = h
        .ok("calendar_events_search", json!({ "query": "tofino" }))
        .await;
    assert_eq!(found["events"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn creates_idempotently_with_vancouver_time_zone_data() {
    let h = harness().await;
    let input = json!({
        "idempotencyKey": "create-vet-0000000001",
        "title": "Vet follow-up",
        "start": "2026-12-01T10:00",
        "durationMinutes": 30,
        "location": "Clinic",
        "alarms": [{ "minutesBefore": 60 }],
    });
    let out = h.ok("calendar_event_create", input.clone()).await;
    assert_eq!(out["status"], "created", "{out}");
    assert_eq!(out["state"], "confirmed");
    assert_eq!(out["verified"], true, "{out}");
    let event_id = out["eventId"].as_str().unwrap().to_owned();
    assert!(event_id.starts_with("omni-agent-") && event_id.ends_with(".ics"));
    let body = h.fake.body(&event_id).unwrap();
    assert!(
        body.contains("DTSTART;TZID=America/Vancouver:20261201T100000\r\n"),
        "{body}"
    );
    assert!(
        body.contains("BEGIN:VTIMEZONE\r\nTZID:America/Vancouver\r\n"),
        "{body}"
    );
    assert!(body.contains("TRIGGER:-PT1H\r\n"), "{body}");

    let events = h.list("2026-11-30", "2026-12-02").await;
    assert_eq!(events[0]["startUtc"], "2026-12-01T17:00:00Z");
    assert_eq!(events[0]["timeZone"], "America/Vancouver");

    // Replays return the recorded result without writing again.
    let writes = h.fake.writes().len();
    let again = h.ok("calendar_event_create", input.clone()).await;
    assert_eq!(again["replayed"], true);
    assert_eq!(again["eventId"], event_id.as_str());
    assert_eq!(h.fake.writes().len(), writes);

    let mut other = input.clone();
    other["title"] = json!("Something else");
    let err = h.call("calendar_event_create", other).await.unwrap_err();
    assert!(err.contains("idempotency_key_reused"), "{err}");

    // Summer time gives the same UTC hour in Vancouver's bundled rules.
    let summer = h
        .ok(
            "calendar_event_create",
            json!({ "idempotencyKey": "create-summer-000000001", "title": "Summer", "start": "2026-07-01T10:00" }),
        )
        .await;
    let summer_id = summer["eventId"].as_str().unwrap();
    let detail = h
        .ok(
            "calendar_event_get",
            json!({ "eventId": summer_id, "fresh": true }),
        )
        .await;
    assert_eq!(detail["upcoming"], json!([]));
    let summer_events = h.list("2026-07-01", "2026-07-02").await;
    assert_eq!(summer_events[0]["startUtc"], "2026-07-01T17:00:00Z");

    // The change feed marks the tool's writes as Omni's and skips them by
    // default.
    let external = h.ok("calendar_changes_list", json!({})).await;
    assert_eq!(external["changes"], json!([]), "{external}");
    let feed = h
        .ok("calendar_changes_list", json!({ "origin": "any" }))
        .await;
    let origins: Vec<&str> = feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["origin"].as_str().unwrap())
        .collect();
    assert!(origins.iter().all(|o| *o == "omni"), "{feed}");
}

#[tokio::test]
async fn creates_multi_day_all_day_events_with_an_exclusive_end() {
    let h = harness().await;
    let out = h
        .ok(
            "calendar_event_create",
            json!({ "idempotencyKey": "create-trip-0000000001", "title": "Trip", "start": "2026-10-10", "end": "2026-10-12", "free": true }),
        )
        .await;
    let body = h.fake.body(out["eventId"].as_str().unwrap()).unwrap();
    assert!(body.contains("DTSTART;VALUE=DATE:20261010\r\n"), "{body}");
    assert!(body.contains("DTEND;VALUE=DATE:20261013\r\n"), "{body}");
    assert!(body.contains("TRANSP:TRANSPARENT\r\n"), "{body}");
    assert!(!body.contains("VTIMEZONE"), "{body}");
}

#[tokio::test]
async fn refuses_stale_versions_and_reports_concurrent_edits() {
    let h = harness().await;
    h.fake.put("dentist-1.ics", &dentist());
    let detail = h
        .ok(
            "calendar_event_get",
            json!({ "eventId": "dentist-1.ics", "fresh": true }),
        )
        .await;
    let etag = detail["etag"].as_str().unwrap().to_owned();
    h.fake
        .put("dentist-1.ics", &dentist().replace("Clinic", "Elsewhere"));
    h.fake.clear_requests();
    let err = h
        .call(
            "calendar_event_update",
            json!({ "idempotencyKey": "update-stale-000000001", "eventId": "dentist-1.ics", "etag": etag, "changes": { "title": "Dentist!" } }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("version_conflict"), "{err}");
    assert!(h.fake.writes().is_empty());

    // Someone edits between the read and the conditional PUT: 412.
    h.fake
        .lock()
        .race_next_put
        .insert(format!("{PRIMARY}dentist-1.ics"));
    let err = h
        .call(
            "calendar_event_update",
            json!({ "idempotencyKey": "update-race-0000000001", "eventId": "dentist-1.ics", "changes": { "title": "Dentist!" } }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("version_conflict"), "{err}");
    let status = h
        .ok(
            "calendar_write_status",
            json!({ "idempotencyKey": "update-race-0000000001" }),
        )
        .await;
    assert_eq!(status["state"], "failed");
    assert_eq!(status["error"]["code"], "version_conflict");
    assert!(
        h.fake
            .body("dentist-1.ics")
            .unwrap()
            .contains("SUMMARY:Edited Dentist")
    );
}

#[tokio::test]
async fn reports_a_uid_conflict_from_another_calendar() {
    let h = harness().await;
    let key = "create-conflict-000001";
    let (_, uid) = omni_calendar::primary::edit::derived_ids("calendar-create", key);
    h.fake.lock().foreign_uids.insert(uid);
    let err = h
        .call(
            "calendar_event_create",
            json!({ "idempotencyKey": key, "title": "Dup", "start": "2026-10-20T09:00" }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("uid_conflict"), "{err}");
}

#[tokio::test]
async fn settles_an_uncertain_write_only_by_reading_it_back() {
    let h = harness().await;
    h.ok("calendar_status", json!({})).await;
    h.fake.lock().fail_next_write = Some(500);
    let key = "create-uncertain-00001";
    let out = h
        .ok(
            "calendar_event_create",
            json!({ "idempotencyKey": key, "title": "Maybe", "start": "2026-10-20T09:00" }),
        )
        .await;
    assert_eq!(out["state"], "uncertain", "{out}");
    let writes = h.fake.writes().len();

    let status = h
        .ok("calendar_write_status", json!({ "idempotencyKey": key }))
        .await;
    assert_eq!(status["state"], "uncertain");
    h.later(11 * 60 * 1000);
    let status = h
        .ok("calendar_write_status", json!({ "idempotencyKey": key }))
        .await;
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["error"]["code"], "not_applied");
    assert_eq!(h.fake.writes().len(), writes, "never re-sent");
}

#[tokio::test]
async fn edits_one_occurrence_and_deletes_another() {
    let h = harness().await;
    h.fake.put("weekly-1.ics", &weekly());
    h.list("2026-10-01", "2026-11-15").await;
    let out = h
        .ok(
            "calendar_event_update",
            json!({
                "idempotencyKey": "occurrence-move-000001",
                "eventId": "weekly-1.ics",
                "scope": "occurrence",
                "recurrenceId": "2026-10-26T09:00:00[America/Vancouver]",
                "changes": { "moveStartTo": "2026-10-26T15:00", "title": "Late standup" },
            }),
        )
        .await;
    assert_eq!(out["verified"], true, "{out}");
    let body = h.fake.body("weekly-1.ics").unwrap();
    assert!(
        body.contains("RECURRENCE-ID;TZID=America/Vancouver:20261026T090000\r\n"),
        "{body}"
    );
    assert!(
        body.contains("DTSTART;TZID=America/Vancouver:20261026T150000\r\n"),
        "{body}"
    );
    assert!(
        body.contains("SUMMARY:Standup\r\n"),
        "the series keeps its title: {body}"
    );

    h.ok(
        "calendar_event_delete",
        json!({
            "idempotencyKey": "occurrence-delete-00001",
            "eventId": "weekly-1.ics",
            "scope": "occurrence",
            "recurrenceId": "2026-11-02T09:00:00[America/Vancouver]",
        }),
    )
    .await;
    h.later(60_000);
    let events = h.list("2026-10-01", "2026-11-15").await;
    assert_eq!(
        titles(&events),
        [
            "2026-10-05T09:00:00 Standup",
            "2026-10-19T11:00:00 Standup (moved)",
            "2026-10-26T15:00:00 Late standup",
            "2026-11-09T09:00:00 Standup",
        ]
    );

    let err = h
        .call(
            "calendar_event_delete",
            json!({
                "idempotencyKey": "occurrence-missing-0001",
                "eventId": "weekly-1.ics",
                "scope": "occurrence",
                "recurrenceId": "2026-10-12T09:00:00[America/Vancouver]",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        err.contains("not_found"),
        "an excluded date is no occurrence: {err}"
    );
}

#[tokio::test]
async fn splits_a_series_at_this_and_following() {
    let h = harness().await;
    h.fake.put("weekly-1.ics", &weekly());
    h.list("2026-10-01", "2026-11-15").await;
    let out = h
        .ok(
            "calendar_event_update",
            json!({
                "idempotencyKey": "split-following-000001",
                "eventId": "weekly-1.ics",
                "scope": "following",
                "recurrenceId": "2026-11-02T09:00:00[America/Vancouver]",
                "changes": { "title": "New standup" },
            }),
        )
        .await;
    let new_id = out["eventId"].as_str().unwrap().to_owned();
    assert_ne!(new_id, "weekly-1.ics");
    let original = h.fake.body("weekly-1.ics").unwrap();
    assert!(
        original.contains("RRULE:FREQ=WEEKLY;UNTIL=20261102T155959Z\r\n"),
        "{original}"
    );
    let split = h.fake.body(&new_id).unwrap();
    assert!(split.contains("RRULE:FREQ=WEEKLY;COUNT=2\r\n"), "{split}");
    assert!(
        split.contains("RELATED-TO;RELTYPE=SIBLING:weekly-1\r\n"),
        "{split}"
    );
    assert!(split.contains("SUMMARY:New standup\r\n"), "{split}");

    h.later(60_000);
    let events = h.list("2026-10-01", "2026-11-15").await;
    assert_eq!(
        titles(&events),
        [
            "2026-10-05T09:00:00 Standup",
            "2026-10-19T11:00:00 Standup (moved)",
            "2026-10-26T09:00:00 Standup",
            "2026-11-02T09:00:00 New standup",
            "2026-11-09T09:00:00 New standup",
        ]
    );
}

#[tokio::test]
async fn keeps_invitations_read_only_and_refuses_silent_attendee_email() {
    let h = harness().await;
    h.fake.put(
        "invite-1.ics",
        &calendar(&vevent(&[
            "UID:invite-1",
            "SUMMARY:Planning",
            "DTSTART;TZID=America/Vancouver:20261015T100000",
            "DTEND;TZID=America/Vancouver:20261015T110000",
            "ORGANIZER;CN=Boss:mailto:boss@example.com",
            "ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:michael@thiesen.dev",
        ])),
    );
    h.fake.put(
        "party-1.ics",
        &calendar(&vevent(&[
            "UID:party-1",
            "SUMMARY:Party",
            "DTSTART;TZID=America/Vancouver:20261016T180000",
            "DTEND;TZID=America/Vancouver:20261016T220000",
            "ORGANIZER:mailto:michael@thiesen.dev",
            "ATTENDEE;PARTSTAT=ACCEPTED:mailto:friend@example.com",
        ])),
    );
    let err = h
        .call(
            "calendar_event_update",
            json!({ "idempotencyKey": "invite-edit-0000000001", "eventId": "invite-1.ics", "changes": { "title": "Mine" } }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("invitation_read_only"), "{err}");
    let err = h
        .call(
            "calendar_event_delete",
            json!({ "idempotencyKey": "party-delete-000000001", "eventId": "party-1.ics" }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("scheduling_side_effect_refused"), "{err}");
    assert!(h.fake.writes().is_empty());
    let out = h
        .ok(
            "calendar_event_update",
            json!({ "idempotencyKey": "party-edit-00000000001", "eventId": "party-1.ics", "changes": { "location": "Home" }, "attendeeNotifications": "send" }),
        )
        .await;
    assert_eq!(out["schedulingNotified"], true);
}

#[tokio::test]
async fn an_override_with_attendees_needs_send_for_series_edits() {
    let h = harness().await;
    h.fake.put(
        "weekly-guests.ics",
        &calendar(&format!(
            "{}{}",
            vevent(&[
                "UID:weekly-guests",
                "SUMMARY:Run",
                "DTSTART;TZID=America/Vancouver:20261005T070000",
                "DTEND;TZID=America/Vancouver:20261005T080000",
                "RRULE:FREQ=WEEKLY;COUNT=4",
            ]),
            vevent(&[
                "UID:weekly-guests",
                "RECURRENCE-ID;TZID=America/Vancouver:20261012T070000",
                "SUMMARY:Run",
                "DTSTART;TZID=America/Vancouver:20261012T070000",
                "DTEND;TZID=America/Vancouver:20261012T080000",
                "ORGANIZER:mailto:michael@thiesen.dev",
                "ATTENDEE;PARTSTAT=ACCEPTED:mailto:friend@example.com",
            ]),
        )),
    );
    let err = h
        .call(
            "calendar_event_update",
            json!({ "idempotencyKey": "guests-edit-0000000001", "eventId": "weekly-guests.ics", "changes": { "title": "Long run" } }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("scheduling_side_effect_refused"), "{err}");
    let err = h
        .call(
            "calendar_event_delete",
            json!({ "idempotencyKey": "guests-delete-00000001", "eventId": "weekly-guests.ics" }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("scheduling_side_effect_refused"), "{err}");
    assert!(h.fake.writes().is_empty());
}

#[tokio::test]
async fn a_default_href_without_its_trailing_slash_still_matches() {
    let h = harness().await;
    h.fake.lock().default_calendar = Some(PRIMARY.trim_end_matches('/').to_owned());
    let status = h.ok("calendar_status", json!({})).await;
    assert_eq!(status["state"], "ready", "{status}");
    assert_eq!(status["isServerDefault"], true, "{status}");
}

#[tokio::test]
async fn polling_the_status_does_not_postpone_settling() {
    let h = harness().await;
    h.ok("calendar_status", json!({})).await;
    h.fake.lock().fail_next_write = Some(500);
    let key = "create-uncertain-poll01";
    let out = h
        .ok(
            "calendar_event_create",
            json!({ "idempotencyKey": key, "title": "Maybe", "start": "2026-10-20T09:00" }),
        )
        .await;
    assert_eq!(out["state"], "uncertain", "{out}");
    let writes = h.fake.writes().len();
    for _ in 0..3 {
        h.later(4 * 60 * 1000);
        h.ok("calendar_write_status", json!({ "idempotencyKey": key }))
            .await;
    }
    let status = h
        .ok("calendar_write_status", json!({ "idempotencyKey": key }))
        .await;
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["error"]["code"], "not_applied");
    assert_eq!(h.fake.writes().len(), writes, "never re-sent");
}

#[tokio::test]
async fn previews_without_writing() {
    let h = harness().await;
    h.fake.put("dentist-1.ics", &dentist());
    let out = h
        .ok(
            "calendar_event_preview",
            json!({ "update": { "idempotencyKey": "preview-only-00000001", "eventId": "dentist-1.ics", "changes": { "moveStartTo": "2026-10-09" } } }),
        )
        .await;
    assert_eq!(out["status"], "updated");
    assert_eq!(out["changedFields"], json!(["end", "start"]));
    assert_eq!(out["writes"][0]["method"], "PUT");
    assert!(
        out["iCalendar"][0]
            .as_str()
            .unwrap()
            .contains("20261009T140000")
    );
    assert!(h.fake.writes().is_empty());
}

#[tokio::test]
async fn serves_status_events_and_changes_over_the_api() {
    let h = harness().await;
    h.fake.put("dentist-1.ics", &dentist());
    let router = omni_calendar::routes::router(h.service.clone());
    let (status, events) = h
        .app
        .get_json(
            &router,
            "/api/calendar/events?from=2026-10-01&to=2026-10-31",
        )
        .await;
    assert_eq!(status, 200, "{events}");
    assert_eq!(events["events"][0]["title"], "Dentist");
    let (status, body) = h.app.get_json(&router, "/api/calendar/status").await;
    assert_eq!(status, 200);
    assert_eq!(body["state"], "ready");
    let (status, body) = h
        .app
        .get_json(&router, "/api/calendar/changes?cursor=0")
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["changes"], json!([]));
    let (status, _) = h
        .app
        .get_json(
            &router,
            "/api/calendar/events?from=2026-10-31&to=2026-10-01",
        )
        .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn boot_marks_interrupted_writes_uncertain_without_sending() {
    let h = harness().await;
    let step = |state| OperationStep {
        kind: StepKind::Put,
        event_id: "x.ics".to_owned(),
        precondition: "if-none-match".to_owned(),
        body_sha256: None,
        state,
        result_etag: None,
        http_status: None,
        detail: None,
        before_ics: None,
    };
    let record = |key: &str, state| OperationRecord {
        key_hash: key.to_owned(),
        idempotency_key: key.to_owned(),
        fingerprint: String::new(),
        tool: "calendar_event_create".to_owned(),
        state: OperationState::Reserved,
        steps: vec![step(state)],
        planned_bodies: Default::default(),
        result: None,
        error: None,
        created_at: 0,
        updated_at: 0,
        extra: Default::default(),
    };
    let sending = record("sending", StepState::Sending);
    let planned = record("planned", StepState::Planned);
    h.app
        .ctx
        .store
        .write(move |tx| {
            tx.upsert(&sending, UpsertOpts::default())?;
            tx.upsert(&planned, UpsertOpts::default())
        })
        .await
        .unwrap();
    let settled =
        omni_calendar::primary::operations::reconcile_interrupted(&h.app.ctx.store, NOW_MS)
            .await
            .unwrap();
    assert_eq!(settled, 2);
    let rows = h
        .app
        .ctx
        .store
        .read(|docs| docs.get_all::<OperationRecord>())
        .await
        .unwrap();
    let state_of = |k: &str| rows.iter().find(|r| r.key_hash == k).unwrap().state;
    assert_eq!(state_of("sending"), OperationState::Uncertain);
    assert_eq!(state_of("planned"), OperationState::Failed);
    assert!(h.fake.writes().is_empty());
}
