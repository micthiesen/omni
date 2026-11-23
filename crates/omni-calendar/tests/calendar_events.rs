//! `calendar.event_changed` and `calendar.event_starting` publications
//! against a fake CalDAV server and a recording MCP Events port.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::fake_caldav;

use std::sync::Arc;

use fake_caldav::FakeCaldav;
use omni_api::events::{CALENDAR_EVENT_CHANGED, CALENDAR_EVENT_STARTING};
use omni_calendar::caldav::CaldavSettings;
use omni_calendar::mcp::{CalendarTools, calendar_tools};
use omni_calendar::primary::store::{SINGLETON, SyncState};
use omni_calendar::primary::{PrimaryCalendar, PrimaryDeps, starting};
use omni_core::clock::TestClock;
use omni_http::SideEffectMode;
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput};
use omni_runtime::ports::EventPublication;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_testkit::{RecordedEvents, TestApp};
use serde_json::{Value, json};

fn at(instant: &str) -> i64 {
    instant.parse::<jiff::Timestamp>().unwrap().as_millisecond()
}

struct Harness {
    app: TestApp,
    fake: FakeCaldav,
    clock: Arc<TestClock>,
    service: PrimaryCalendar,
    tools: Vec<McpTool>,
    events: RecordedEvents,
}

async fn harness(now: &str) -> Harness {
    let app = TestApp::new().await;
    let events = RecordedEvents::install(&app.ctx.ports);
    let fake = FakeCaldav::start().await;
    let clock = omni_testkit::test_clock(at(now));
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
        events,
    }
}

impl Harness {
    async fn ok(&self, name: &str, input: Value) -> Value {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        match tool.handler.call(input, standalone_context("test")).await {
            Ok(ToolOutput::Structured(map)) => Value::Object(map),
            Ok(ToolOutput::Custom { structured, .. }) => Value::Object(structured),
            Err(e) => panic!("{name} failed: {}", e.message),
        }
    }

    fn set(&self, instant: &str) {
        self.clock.set(at(instant));
    }

    /// Syncs as the background task would after the mirror aged.
    async fn sync_later(&self) {
        self.clock
            .set(omni_core::clock::Clock::now_ms(self.clock.as_ref()) + 60_000);
        self.service.sync_now().await.unwrap();
    }

    fn named(&self, name: &str) -> Vec<EventPublication> {
        self.events
            .published()
            .into_iter()
            .filter(|e| e.name == name)
            .collect()
    }

    async fn published_seq(&self) -> i64 {
        self.app
            .ctx
            .store
            .read(|docs| docs.get::<SyncState>(&SINGLETON.to_owned()))
            .await
            .unwrap()
            .unwrap()
            .published_seq
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

/// 2026-10-08 14:00-15:00 Vancouver (21:00Z).
fn dentist() -> String {
    calendar(&vevent(&[
        "UID:dentist-1",
        "SUMMARY:Dentist",
        "LOCATION:Clinic",
        "DTSTART;TZID=America/Vancouver:20261008T140000",
        "DTEND;TZID=America/Vancouver:20261008T150000",
    ]))
}

/// Weekly 09:00 Vancouver from 2026-10-05, six times, without 10-12.
fn weekly() -> String {
    calendar(&vevent(&[
        "UID:weekly-1",
        "SUMMARY:Standup",
        "DTSTART;TZID=America/Vancouver:20261005T090000",
        "DTEND;TZID=America/Vancouver:20261005T093000",
        "RRULE:FREQ=WEEKLY;COUNT=6",
        "EXDATE;TZID=America/Vancouver:20261012T090000",
    ]))
}

fn trip() -> String {
    calendar(&vevent(&[
        "UID:trip-1",
        "SUMMARY:Tofino trip",
        "DTSTART;VALUE=DATE:20261010",
        "DTEND;VALUE=DATE:20261013",
    ]))
}

fn with_alarm(acknowledged: Option<&str>) -> String {
    let mut alarm = String::from(
        "BEGIN:VALARM\r\nX-WR-ALARMUID:alarm-1\r\nACTION:DISPLAY\r\nTRIGGER:-PT30M\r\n",
    );
    if let Some(at) = acknowledged {
        alarm.push_str(&format!("ACKNOWLEDGED:{at}\r\n"));
    }
    alarm.push_str("END:VALARM\r\n");
    calendar(&format!(
        "BEGIN:VEVENT\r\nDTSTAMP:20260901T000000Z\r\nUID:vet-1\r\nSUMMARY:Vet\r\nDTSTART;TZID=America/Vancouver:20261008T100000\r\nDTEND;TZID=America/Vancouver:20261008T103000\r\n{alarm}END:VEVENT\r\n"
    ))
}

// ---- calendar.event_changed ----

#[tokio::test]
async fn publishes_external_changes_after_a_silent_baseline() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_CHANGED, &[]);
    h.fake.put("dentist-1.ics", &dentist());
    h.fake.put("weekly-1.ics", &weekly());
    h.service.sync_now().await.unwrap();
    assert!(h.named(CALENDAR_EVENT_CHANGED).is_empty(), "baseline");

    h.fake
        .put("dentist-1.ics", &dentist().replace("Clinic", "New clinic"));
    let removed_etag = h.fake.etag("weekly-1.ics").unwrap();
    h.fake.remove("weekly-1.ics");
    let created_etag = h.fake.put("trip-1.ics", &trip());
    h.sync_later().await;

    let published = h.named(CALENDAR_EVENT_CHANGED);
    assert_eq!(published.len(), 3, "{published:?}");
    let by_kind = |kind: &str| {
        published
            .iter()
            .find(|e| e.data["changeKind"] == kind)
            .unwrap()
            .clone()
    };
    let updated = by_kind("updated");
    assert_eq!(updated.data["eventId"], "dentist-1.ics");
    assert_eq!(updated.data["changedFields"], json!(["location"]));
    assert_eq!(updated.data["summary"], "Dentist");
    assert_eq!(updated.data["start"], "2026-10-08T21:00:00Z");
    assert_eq!(updated.data["origin"], "external");
    let version = updated.data["version"].as_str().unwrap();
    assert_eq!(updated.dedup_key, format!("dentist-1.ics:{version}"));

    let deleted = by_kind("deleted");
    assert_eq!(deleted.data["summary"], "Standup");
    assert_eq!(deleted.data["recurring"], true);
    assert_eq!(deleted.data["version"], Value::Null);
    assert_eq!(deleted.data["changedFields"], json!([]));
    assert_eq!(
        deleted.dedup_key,
        format!("weekly-1.ics:deleted:{removed_etag}")
    );

    let created = by_kind("created");
    assert_eq!(created.data["allDay"], true);
    assert_eq!(created.data["uid"], "trip-1");
    assert_eq!(created.dedup_key, format!("trip-1.ics:{created_etag}"));

    // Nothing new: the next sync publishes nothing.
    h.sync_later().await;
    assert_eq!(h.named(CALENDAR_EVENT_CHANGED).len(), 3);
}

#[tokio::test]
async fn a_recurring_change_reports_the_next_occurrence() {
    let h = harness("2026-10-14T16:00:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_CHANGED, &[]);
    h.fake.put("weekly-1.ics", &weekly());
    h.service.sync_now().await.unwrap();
    h.fake
        .put("weekly-1.ics", &weekly().replace("Standup", "Team standup"));
    h.sync_later().await;
    let published = h.named(CALENDAR_EVENT_CHANGED);
    assert_eq!(published[0].data["summary"], "Team standup");
    // 2026-10-19 09:00 Vancouver, not the series' first start on 10-05.
    assert_eq!(published[0].data["start"], "2026-10-19T16:00:00Z");
}

#[tokio::test]
async fn tags_tool_writes_as_omni_and_publishes_them_for_matching() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.events
        .subscribe(CALENDAR_EVENT_CHANGED, &[("origin", "any")]);
    h.service.sync_now().await.unwrap();
    h.ok(
        "calendar_event_create",
        json!({
            "idempotencyKey": "create-omni-echo-000001",
            "title": "Haircut",
            "start": "2026-10-20T10:00",
        }),
    )
    .await;
    h.sync_later().await;
    let published = h.named(CALENDAR_EVENT_CHANGED);
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0].data["origin"], "omni");
    assert_eq!(published[0].data["changeKind"], "created");
}

#[tokio::test]
async fn without_a_subscriber_the_cursor_skips_ahead_and_replays_keep_event_ids() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.fake.put("dentist-1.ics", &dentist());
    h.service.sync_now().await.unwrap();
    h.fake
        .put("dentist-1.ics", &dentist().replace("Dentist", "Dentist 2"));
    h.sync_later().await;
    assert!(h.events.published().is_empty());
    assert_eq!(h.published_seq().await, 1, "skipped while unsubscribed");

    // Subscribing later does not deliver history.
    h.events.subscribe(CALENDAR_EVENT_CHANGED, &[]);
    h.sync_later().await;
    assert!(h.events.published().is_empty());

    h.fake
        .put("dentist-1.ics", &dentist().replace("Dentist", "Dentist 3"));
    h.sync_later().await;
    assert_eq!(h.named(CALENDAR_EVENT_CHANGED).len(), 1);

    // A crash before the cursor advanced replays the same dedup key.
    h.app
        .ctx
        .store
        .write(|tx| {
            let mut state = tx.get::<SyncState>(&SINGLETON.to_owned())?.unwrap();
            state.published_seq = 1;
            tx.upsert(&state, UpsertOpts::default())
        })
        .await
        .unwrap();
    h.sync_later().await;
    assert_eq!(h.named(CALENDAR_EVENT_CHANGED).len(), 2);
    assert_eq!(
        h.events.distinct().len(),
        1,
        "the replay keeps its event ID"
    );
}

#[tokio::test]
async fn a_failing_port_keeps_the_cursor_for_the_next_sync() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_CHANGED, &[]);
    h.fake.put("dentist-1.ics", &dentist());
    h.service.sync_now().await.unwrap();
    h.events.fail();
    h.fake
        .put("dentist-1.ics", &dentist().replace("Dentist", "Dentist 2"));
    h.sync_later().await;
    assert_eq!(h.published_seq().await, 0);
    assert!(h.events.published().is_empty());
}

#[tokio::test]
async fn a_sync_during_a_tool_write_still_tags_it_as_omni() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.events
        .subscribe(CALENDAR_EVENT_CHANGED, &[("origin", "any")]);
    h.service.sync_now().await.unwrap();
    let gate = fake_caldav::WriteGate::default();
    h.fake.lock().hold_next_write = Some(gate.clone());
    let write = h.ok(
        "calendar_event_create",
        json!({
            "idempotencyKey": "create-during-sync-0001",
            "title": "Haircut",
            "start": "2026-10-20T10:00",
        }),
    );
    let racer = async {
        // The PUT is applied but not yet answered, so no echo exists yet.
        gate.applied.notified().await;
        let sync = h.service.sync_now();
        tokio::pin!(sync);
        let early = tokio::time::timeout(std::time::Duration::from_millis(300), &mut sync).await;
        gate.release.notify_one();
        match early {
            Ok(result) => result.unwrap(),
            Err(_) => sync.await.unwrap(),
        };
    };
    tokio::join!(write, racer);
    h.sync_later().await;
    let published = h.named(CALENDAR_EVENT_CHANGED);
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0].data["origin"], "omni");
}

#[tokio::test]
async fn the_polling_fallback_filters_by_origin() {
    let h = harness("2026-10-01T16:00:00Z").await;
    h.fake.put("dentist-1.ics", &dentist());
    h.service.sync_now().await.unwrap();
    h.ok(
        "calendar_event_create",
        json!({
            "idempotencyKey": "create-omni-echo-000002",
            "title": "Haircut",
            "start": "2026-10-20T10:00",
        }),
    )
    .await;
    h.sync_later().await;
    h.fake
        .put("dentist-1.ics", &dentist().replace("Clinic", "New clinic"));
    h.sync_later().await;

    let external = h.ok("calendar_changes_list", json!({})).await;
    let changes = external["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{external}");
    assert_eq!(changes[0]["eventId"], "dentist-1.ics");
    assert_eq!(changes[0]["uid"], "dentist-1");
    assert!(changes[0]["version"].is_string());
    assert_eq!(external["nextCursor"], "2");

    let any = h
        .ok(
            "calendar_changes_list",
            json!({ "origin": "any", "limit": 1 }),
        )
        .await;
    assert_eq!(any["changes"][0]["origin"], "omni");
    assert_eq!(any["hasMore"], true);
    assert_eq!(any["nextCursor"], "1");

    // The cursor moves past filtered rows.
    let after_omni = h
        .ok(
            "calendar_changes_list",
            json!({ "cursor": "0", "limit": 1 }),
        )
        .await;
    assert_eq!(after_omni["changes"][0]["eventId"], "dentist-1.ics");
    assert_eq!(after_omni["hasMore"], false);
    assert_eq!(after_omni["nextCursor"], "2");
}

// ---- calendar.event_starting ----

async fn scan(h: &Harness) -> Vec<EventPublication> {
    starting::scan(&h.service).await.unwrap();
    h.named(CALENDAR_EVENT_STARTING)
}

#[tokio::test]
async fn costs_nothing_without_subscribers() {
    let h = harness("2026-10-08T20:45:00Z").await;
    h.fake.put("dentist-1.ics", &dentist());
    assert_eq!(starting::scan(&h.service).await.unwrap(), 0);
    assert!(
        h.fake.lock().requests.is_empty(),
        "no sync without a subscriber"
    );
}

#[tokio::test]
async fn fires_once_at_the_lead_time_and_again_after_a_reschedule() {
    let h = harness("2026-10-08T20:40:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.fake.put("dentist-1.ics", &dentist());
    assert!(scan(&h).await.is_empty(), "before the 15-minute lead");

    h.set("2026-10-08T20:45:10Z");
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    let event = &fired[0].data;
    assert_eq!(event["eventId"], "dentist-1.ics");
    assert_eq!(event["trigger"], "start");
    assert_eq!(event["leadMinutes"], "15");
    assert_eq!(event["start"], "2026-10-08T21:00:00Z");
    assert_eq!(event["end"], "2026-10-08T22:00:00Z");
    assert_eq!(event["fireAt"], "2026-10-08T20:45:00Z");
    assert_eq!(event["late"], false);
    assert_eq!(event["hasLocation"], true);
    assert_eq!(event["timeZone"], "America/Vancouver");
    assert_eq!(event["recurrenceId"], Value::Null);
    assert_eq!(event["includeAllDay"], "false");

    h.set("2026-10-08T20:45:40Z");
    scan(&h).await;
    assert_eq!(h.events.distinct().len(), 1, "rescans are deduplicated");

    // Moved to 14:30: the new start fires again.
    h.fake.put(
        "dentist-1.ics",
        &dentist()
            .replace("T140000", "T143000")
            .replace("T150000", "T153000"),
    );
    h.set("2026-10-08T21:15:05Z");
    scan(&h).await;
    let distinct = h.events.distinct();
    assert_eq!(distinct.len(), 2);
    assert_eq!(distinct[1].data["start"], "2026-10-08T21:30:00Z");
}

#[tokio::test]
async fn publishes_once_per_subscription_tuple() {
    let h = harness("2026-10-08T20:45:10Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.events.subscribe(
        CALENDAR_EVENT_STARTING,
        &[("trigger", "start"), ("leadMinutes", "0")],
    );
    h.fake.put("dentist-1.ics", &dentist());
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1, "{fired:?}");
    assert_eq!(fired[0].data["leadMinutes"], "15");

    // A zero lead fires at the start, while the occurrence has begun.
    h.set("2026-10-08T21:00:20Z");
    scan(&h).await;
    let distinct = h.events.distinct();
    assert_eq!(distinct.len(), 2, "{distinct:?}");
    assert_eq!(distinct[1].data["leadMinutes"], "0");
    assert_eq!(distinct[1].data["late"], false);
}

#[tokio::test]
async fn late_starts_fire_only_while_the_occurrence_lies_ahead() {
    let h = harness("2026-10-08T20:55:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.fake.put("dentist-1.ics", &dentist());
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].data["late"], true, "Omni was down at 20:45");

    let missed = harness("2026-10-08T21:05:00Z").await;
    missed.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    missed.fake.put("dentist-1.ics", &dentist());
    assert!(scan(&missed).await.is_empty(), "already started");
}

#[tokio::test]
async fn exdates_suppress_occurrences() {
    // 2026-10-12 09:00 Vancouver (UTC-7) is 16:00Z.
    let h = harness("2026-10-12T15:45:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.fake.put("weekly-1.ics", &weekly());
    assert!(scan(&h).await.is_empty(), "10-12 is an EXDATE");
    h.set("2026-10-19T15:45:00Z");
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    assert_eq!(
        fired[0].data["recurrenceId"],
        "2026-10-19T09:00:00[America/Vancouver]"
    );
}

#[tokio::test]
async fn lead_times_follow_the_events_zone_across_a_dst_change() {
    // Toronto still leaves daylight time on 2026-11-01 (Vancouver no longer does).
    let toronto = calendar(&vevent(&[
        "UID:gym-1",
        "SUMMARY:Gym",
        "DTSTART;TZID=America/Toronto:20261026T090000",
        "DTEND;TZID=America/Toronto:20261026T100000",
        "RRULE:FREQ=WEEKLY;COUNT=3",
    ]));
    let h = harness("2026-10-26T12:45:00Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.fake.put("gym-1.ics", &toronto);
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].data["start"], "2026-10-26T13:00:00Z");

    h.set("2026-11-02T12:45:00Z");
    assert_eq!(scan(&h).await.len(), 1, "an hour early in EST");
    h.set("2026-11-02T13:45:00Z");
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 2);
    assert_eq!(fired[1].data["start"], "2026-11-02T14:00:00Z");
    assert_eq!(fired[1].data["fireAt"], "2026-11-02T13:45:00Z");
    assert_eq!(fired[1].data["timeZone"], "America/Toronto");
}

#[tokio::test]
async fn all_day_occurrences_need_include_all_day() {
    // 2026-10-10 00:00 Vancouver is 07:00Z; a 60-minute lead fires at 06:00Z.
    let h = harness("2026-10-10T06:00:30Z").await;
    h.events.subscribe(
        CALENDAR_EVENT_STARTING,
        &[("trigger", "start"), ("leadMinutes", "60")],
    );
    h.fake.put("trip-1.ics", &trip());
    assert!(scan(&h).await.is_empty());

    h.events.subscribe(
        CALENDAR_EVENT_STARTING,
        &[
            ("trigger", "start"),
            ("leadMinutes", "60"),
            ("includeAllDay", "true"),
        ],
    );
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].data["allDay"], true);
    assert_eq!(fired[0].data["includeAllDay"], "true");
    assert_eq!(fired[0].data["start"], "2026-10-10T07:00:00Z");
}

#[tokio::test]
async fn alarms_fire_at_their_trigger_unless_acknowledged() {
    // 10:00 Vancouver is 17:00Z; the alarm is 30 minutes before.
    let h = harness("2026-10-08T16:30:20Z").await;
    h.events
        .subscribe(CALENDAR_EVENT_STARTING, &[("trigger", "alarm")]);
    h.fake.put("vet-1.ics", &with_alarm(None));
    let fired = scan(&h).await;
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].data["trigger"], "alarm");
    assert_eq!(fired[0].data["alarmId"], "alarm-1");
    assert_eq!(fired[0].data["leadMinutes"], Value::Null);
    assert_eq!(fired[0].data["fireAt"], "2026-10-08T16:30:00Z");

    let acked = harness("2026-10-08T16:31:00Z").await;
    acked
        .events
        .subscribe(CALENDAR_EVENT_STARTING, &[("trigger", "alarm")]);
    acked
        .fake
        .put("vet-1.ics", &with_alarm(Some("20261008T163040Z")));
    assert!(scan(&acked).await.is_empty(), "dismissed on a device");

    let stale = harness("2026-10-08T16:45:00Z").await;
    stale
        .events
        .subscribe(CALENDAR_EVENT_STARTING, &[("trigger", "alarm")]);
    stale.fake.put("vet-1.ics", &with_alarm(None));
    assert!(scan(&stale).await.is_empty(), "15 minutes late is history");
}

#[tokio::test]
async fn cancelled_occurrences_never_fire() {
    let h = harness("2026-10-08T20:45:10Z").await;
    h.events.subscribe(CALENDAR_EVENT_STARTING, &[]);
    h.fake.put(
        "dentist-1.ics",
        &dentist().replace("UID:dentist-1", "UID:dentist-1\r\nSTATUS:CANCELLED"),
    );
    assert!(scan(&h).await.is_empty());
}

#[tokio::test]
async fn the_sync_task_runs_every_minute_only_while_subscribed() {
    use omni_calendar::primary::{BACKGROUND_MAX_AGE_MS, task};
    let h = harness("2026-10-01T16:00:00Z").await;
    assert_eq!(task::max_age_ms(&h.service).await, BACKGROUND_MAX_AGE_MS);
    h.events
        .subscribe(CALENDAR_EVENT_STARTING, &[("trigger", "alarm")]);
    assert_eq!(
        task::max_age_ms(&h.service).await,
        task::SUBSCRIBED_MAX_AGE_MS
    );
}
