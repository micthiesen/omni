//! An event the email pipeline created stays deleted once it is deleted from
//! the calendar: processing its email again, or an update for it, never
//! writes it back (the production duplicate: a cleaning reminder that returned
//! after each delete). Only that exact event is suppressed; a new date, a
//! reschedule and a rebooked slot still reach the calendar. Runs the real
//! pipeline and calendar tools against the fake CalDAV server.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod fake_caldav;

use std::sync::Arc;

use common::{FakeSupport, email, extraction, pipeline, settings};
use fake_caldav::{FakeCaldav, OTHER, PRIMARY};
use omni_ai::ModelRole;
use omni_calendar::caldav::Caldav;
use omni_calendar::mcp::{CalendarTools, calendar_tools};
use omni_calendar::persistence::{
    self, CreatedCalendarEvent, compute_calendar_event_uid, compute_event_hash,
};
use omni_calendar::pipeline::CalendarEventPipeline;
use omni_calendar::primary::{PrimaryCalendar, PrimaryDeps};
use omni_http::SideEffectMode;
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput};
use omni_store::StoreError;
use omni_store::entity::pk;
use omni_testkit::TestApp;
use serde_json::{Value, json};

const SENDER: &str = "noreply@eventbrite.com";
const COLLECTION: &str = "https://p42-caldav.icloud.com/123/calendars/work/";
const TITLE: &str = "Cleaning";
const DATE: &str = "2026-01-20";

struct Harness {
    app: TestApp,
    fake: FakeCaldav,
    support: Arc<FakeSupport>,
    pipeline: CalendarEventPipeline,
    tools: Vec<McpTool>,
}

async fn harness() -> Harness {
    let app = TestApp::new().await;
    let fake = FakeCaldav::start().await;
    let service = PrimaryCalendar::new(PrimaryDeps {
        http: fake.http(),
        settings: Some(settings(None)),
        store: app.ctx.store.clone(),
        clock: app.ctx.clock.clone(),
        mode: SideEffectMode::Live,
        recorded: Arc::default(),
        default_tz: "America/Vancouver".to_owned(),
        tracker: None,
        ports: app.ctx.ports.clone(),
    });
    let tools = calendar_tools(CalendarTools {
        store: app.ctx.store.clone(),
        primary: service,
    })
    .unwrap();
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let caldav = Caldav::new(
        fake.http(),
        app.ctx.clock.clone(),
        SideEffectMode::Live,
        Some(settings(Some(COLLECTION))),
        "America/Vancouver",
    );
    let pipeline = pipeline(&app, caldav, support.clone(), app.ctx.ai.clone());
    Harness {
        app,
        fake,
        support,
        pipeline,
        tools,
    }
}

fn cleaning(time: &str) -> Value {
    json!({ "action": "create", "title": TITLE, "startDate": DATE, "startTime": time, "allDay": false })
}

fn uid(time: &str) -> String {
    compute_calendar_event_uid(&compute_event_hash(TITLE, DATE, Some(time)))
}

impl Harness {
    /// Processes one email whose extraction yields `events`; returns its
    /// activity item lines.
    async fn process(&self, email_id: &str, events: Value) -> Vec<String> {
        self.app
            .ai
            .script(ModelRole::CalendarExtraction, vec![extraction(events)]);
        let before = self.support.activity().len();
        self.pipeline
            .handle_emails(&[email(
                email_id,
                SENDER,
                "Your cleaning appointment",
                "See you then.",
            )])
            .await
            .unwrap();
        self.support.activity()[before]
            .items
            .clone()
            .unwrap_or_default()
    }

    async fn call(&self, name: &str, input: Value) -> Result<Value, String> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        match tool.handler.call(input, standalone_context("test")).await {
            Ok(ToolOutput::Structured(map)) => Ok(Value::Object(map)),
            Ok(ToolOutput::Custom { structured, .. }) => Ok(Value::Object(structured)),
            Err(e) => Err(e.message),
        }
    }

    async fn delete(&self, key: &str, event_id: &str) -> Result<Value, String> {
        self.call(
            "calendar_event_delete",
            json!({ "idempotencyKey": key, "eventId": event_id }),
        )
        .await
    }

    async fn row(&self, time: &str) -> CreatedCalendarEvent {
        persistence::get_tracked_event(
            &self.app.ctx.store,
            &compute_event_hash(TITLE, DATE, Some(time)),
        )
        .await
        .unwrap()
        .unwrap()
    }

    fn count(&self, method: &str) -> usize {
        self.fake
            .writes()
            .iter()
            .filter(|w| w.method == method)
            .count()
    }

    async fn exec(&self, sql: String) {
        self.app
            .ctx
            .store
            .write(move |tx| {
                tx.connection()
                    .execute_batch(&sql)
                    .map_err(|e| StoreError::Sqlite(e.to_string()))
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_deleted_event_is_not_recreated_when_its_email_is_processed_again() {
    let h = harness().await;
    let items = h.process("notice-1", json!([cleaning("09:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: created"]);
    let name = format!("{}.ics", uid("09:00"));
    assert_eq!(h.fake.names(), std::slice::from_ref(&name));

    let out = h.delete("delete-cleaning-0001", &name).await.unwrap();
    assert_eq!(out["state"], "confirmed", "{out}");
    assert!(h.fake.names().is_empty());
    assert!(h.row("09:00").await.is_deleted());

    // The same email again, and a second notice for the same appointment.
    for email_id in ["notice-1", "notice-2"] {
        let items = h.process(email_id, json!([cleaning("09:00")])).await;
        assert_eq!(
            items,
            ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
        );
    }
    // An update the model addresses by title falls through to the same check.
    let mut update = cleaning("09:00");
    update["action"] = json!("update");
    update["location"] = json!("Suite 4");
    let items = h.process("notice-3", json!([update])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
    );
    assert!(h.fake.names().is_empty());
    assert_eq!(h.count("PUT"), 1, "only the original create was written");
    assert!(h.row("09:00").await.is_deleted());
    assert_eq!(
        h.app.pushes.all().len(),
        1,
        "only the original create notified"
    );
}

#[tokio::test]
async fn only_the_exact_deleted_event_is_suppressed() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    h.delete("delete-cleaning-0002", &name).await.unwrap();

    // Another time for the appointment is a new event.
    let items = h.process("notice-2", json!([cleaning("13:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: created"]);
    // So is the next appointment with the same title.
    let next = json!({ "action": "create", "title": TITLE, "startDate": "2026-02-17", "startTime": "09:00", "allDay": false });
    let items = h.process("notice-3", json!([next])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-02-17: created"]);

    assert_eq!(h.fake.names().len(), 2);
    assert!(!h.fake.names().contains(&name));
    assert!(h.row("09:00").await.is_deleted());
    assert!(!h.row("13:00").await.is_cancelled());
}

#[tokio::test]
async fn a_lost_delete_acknowledgement_still_records_the_deletion() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));

    // iCloud applies the DELETE but the answer is a 500.
    h.fake.lock().lose_next_write = Some(500);
    let out = h.delete("delete-cleaning-0003", &name).await.unwrap();
    assert_eq!(out["state"], "confirmed", "settled by reading back: {out}");
    assert!(h.fake.names().is_empty());
    assert!(h.row("09:00").await.is_deleted());

    let items = h.process("notice-1", json!([cleaning("09:00")])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
    );
    assert!(h.fake.names().is_empty());
    assert_eq!(h.count("DELETE"), 1, "the delete is never sent twice");
}

#[tokio::test]
async fn a_delete_is_not_confirmed_until_its_tombstone_is_stored() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    let hash = compute_event_hash(TITLE, DATE, Some("09:00"));
    let row_pk = pk::<CreatedCalendarEvent>(&hash).unwrap();
    h.exec(format!(
        "CREATE TRIGGER fail_tombstone BEFORE UPDATE ON blobs WHEN NEW.pk = '{}' BEGIN SELECT RAISE(ABORT, 'forced'); END",
        row_pk.replace('\'', "''")
    ))
    .await;

    let key = "delete-cleaning-0004";
    let error = h.delete(key, &name).await.unwrap_err();
    assert!(!error.is_empty());
    // The whole confirmation rolled back: the operation is not confirmed and
    // the row is untouched.
    let status = h
        .call("calendar_write_status", json!({ "idempotencyKey": key }))
        .await
        .unwrap();
    assert_ne!(status["state"], "confirmed", "{status}");
    assert!(!h.row("09:00").await.is_cancelled());
    // Meanwhile the still-active row keeps a replay from recreating the event.
    let items = h.process("notice-1", json!([cleaning("09:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: duplicate, skipped"]);
    assert!(h.fake.names().is_empty());

    h.exec("DROP TRIGGER fail_tombstone".to_owned()).await;
    // Repeating the key settles by reading; nothing is sent again.
    let out = h.delete(key, &name).await.unwrap();
    assert_eq!(out["state"], "confirmed", "{out}");
    assert_eq!(out["replayed"], true, "{out}");
    assert!(h.row("09:00").await.is_deleted());
    assert_eq!(h.count("DELETE"), 1);
    assert_eq!(h.count("PUT"), 1);
}

#[tokio::test]
async fn deleting_one_occurrence_keeps_the_recurring_series_tracked() {
    let h = harness().await;
    let series = json!({
        "action": "create", "title": "Standup", "startDate": "2026-01-05", "startTime": "09:00",
        "allDay": false, "recurrence": { "frequency": "weekly", "until": "2026-03-30" },
    });
    let items = h.process("series-1", json!([series.clone()])).await;
    assert_eq!(items, ["\"Standup\" on 2026-01-05: created"]);
    let hash = compute_event_hash("Standup", "2026-01-05", Some("09:00"));
    let name = format!("{}.ics", compute_calendar_event_uid(&hash));
    let tracked = || async {
        persistence::get_tracked_event(&h.app.ctx.store, &hash)
            .await
            .unwrap()
            .unwrap()
    };

    let out = h
        .call(
            "calendar_event_delete",
            json!({
                "idempotencyKey": "delete-occurrence-001",
                "eventId": name,
                "scope": "occurrence",
                "recurrenceId": "2026-01-12T09:00:00[America/Vancouver]",
            }),
        )
        .await
        .unwrap();
    assert_eq!(out["state"], "confirmed", "{out}");
    assert!(
        !tracked().await.is_cancelled(),
        "the series is still tracked"
    );
    let body = h.fake.body(&name).unwrap();
    assert!(body.contains("RRULE:"), "{body}");
    assert!(body.contains("EXDATE"), "{body}");

    // The email again leaves the series, and its removed occurrence, alone.
    let items = h.process("series-1", json!([series.clone()])).await;
    assert_eq!(items, ["\"Standup\" on 2026-01-05: duplicate, skipped"]);
    assert_eq!(h.fake.body(&name).unwrap(), body);

    // Deleting the whole series is what retires it.
    let out = h.delete("delete-series-000001", &name).await.unwrap();
    assert_eq!(out["state"], "confirmed", "{out}");
    assert!(tracked().await.is_deleted());
    let items = h.process("series-1", json!([series])).await;
    assert_eq!(
        items,
        ["\"Standup\" on 2026-01-05: deleted from the calendar, not recreated"]
    );
    assert!(h.fake.names().is_empty());
}

#[tokio::test]
async fn an_update_never_recreates_an_event_deleted_in_a_calendar_app() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    // Deleted in Calendar: Omni's tracked row still says active.
    h.fake.remove(&name);

    // A later email changes a detail of the appointment the row describes.
    let mut update = cleaning("09:00");
    update["action"] = json!("update");
    update["eventId"] = json!("evt_1");
    update["location"] = json!("Suite 4");
    let items = h.process("notice-2", json!([update])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
    );
    assert!(h.fake.names().is_empty());
    assert_eq!(h.count("PUT"), 1, "only the original create was written");
    assert!(h.row("09:00").await.is_deleted());

    // From then on the original notice is suppressed too.
    let items = h.process("notice-1", json!([cleaning("09:00")])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
    );
    assert!(h.fake.names().is_empty());
}

#[tokio::test]
async fn a_reschedule_of_a_deleted_event_books_the_new_time_only() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let old = format!("{}.ics", uid("09:00"));
    h.fake.remove(&old);

    // Same day, another time: this would have moved the tracked event.
    let items = h.process("notice-2", json!([cleaning("11:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: created"]);
    assert_eq!(h.fake.names(), [format!("{}.ics", uid("11:00"))]);
    assert!(h.row("09:00").await.is_deleted());
    assert!(!h.row("11:00").await.is_cancelled());
}

#[tokio::test]
async fn a_cancel_and_a_new_time_in_one_email_books_the_new_time() {
    let h = harness().await;
    h.process("booking", json!([cleaning("09:00")])).await;
    let cancel = json!({ "action": "cancel", "eventId": "evt_1", "title": TITLE, "startDate": DATE, "allDay": false });

    let items = h.process("moved", json!([cancel, cleaning("11:00")])).await;
    assert_eq!(
        items,
        [
            "\"Cleaning\" on 2026-01-20: cancelled",
            "\"Cleaning\" on 2026-01-20: created",
        ]
    );
    assert_eq!(h.fake.names(), [format!("{}.ics", uid("11:00"))]);
}

#[tokio::test]
async fn a_vanished_write_calendar_tombstones_nothing() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    h.fake.remove(&format!("{}.ics", uid("09:00")));
    // The account no longer lists the calendar the pipeline writes to.
    h.fake.lock().collections.retain(|c| c.path != PRIMARY);

    let items = h.process("notice-2", json!([cleaning("11:00")])).await;
    assert_eq!(items.len(), 1);
    assert!(items[0].contains("update failed"), "{items:?}");
    assert!(!h.row("09:00").await.is_cancelled(), "still tracked");
    assert_eq!(h.count("PUT"), 1, "only the original create was written");
}

#[tokio::test]
async fn an_event_moved_to_another_calendar_and_date_is_out_of_the_lookup_window() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    {
        let mut state = h.fake.lock();
        let (etag, body) = state.resources.remove(&format!("{PRIMARY}{name}")).unwrap();
        // Michael moved it to another calendar and to March.
        state.resources.insert(
            format!("{OTHER}{name}"),
            (etag, body.replace(":20260120T", ":20260310T")),
        );
    }
    let mut update = cleaning("09:00");
    update["action"] = json!("update");
    update["eventId"] = json!("evt_1");
    update["location"] = json!("Suite 4");
    let items = h.process("notice-2", json!([update])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: deleted from the calendar, not recreated"]
    );
    assert_eq!(h.count("PUT"), 1, "his copy is left alone");
}

#[tokio::test]
async fn an_update_follows_an_event_moved_to_another_calendar() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    {
        let mut state = h.fake.lock();
        let copy = state.resources.remove(&format!("{PRIMARY}{name}")).unwrap();
        // Calendar apps keep the UID, not necessarily the resource name.
        state
            .resources
            .insert(format!("{OTHER}moved-copy.ics"), copy);
    }

    let items = h.process("notice-2", json!([cleaning("11:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: updated"]);
    assert!(h.fake.names().is_empty(), "nothing is recreated at home");
    let body = h.fake.lock().resources[&format!("{OTHER}moved-copy.ics")]
        .1
        .clone();
    assert!(body.contains("T110000"), "{body}");
    assert!(!h.row("11:00").await.is_cancelled());
}

#[tokio::test]
async fn an_unfinished_lookup_neither_recreates_nor_forgets_the_event() {
    let h = harness().await;
    h.process("notice-1", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    h.fake.remove(&name);
    // The other calendar cannot be searched, so "deleted" is not established.
    h.fake.lock().broken_reports.insert(OTHER.to_owned());

    let items = h.process("notice-2", json!([cleaning("11:00")])).await;
    assert_eq!(items.len(), 1);
    assert!(items[0].contains("update failed"), "{items:?}");
    assert!(h.fake.names().is_empty());
    assert!(!h.row("09:00").await.is_cancelled(), "still tracked");
    assert_eq!(h.support.retries().len(), 1, "a 5xx lookup is retried");
}

#[tokio::test]
async fn replaying_an_email_does_not_undo_a_later_reschedule_or_cancellation() {
    let h = harness().await;
    h.process("booking", json!([cleaning("09:00")])).await;
    let name = format!("{}.ics", uid("09:00"));
    // A second email moves it to 11:00 (same day, another time).
    let items = h.process("moved", json!([cleaning("11:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: updated"]);
    let moved = h.fake.body(&name).unwrap();
    assert!(moved.contains("T110000"), "{moved}");

    // The booking email again must not move it back to 09:00.
    let items = h.process("booking", json!([cleaning("09:00")])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: cancelled or moved by a later email, not recreated"]
    );
    assert_eq!(h.fake.body(&name).unwrap(), moved);

    // A third email cancels it; the email that last wrote it cannot revive it.
    let cancel = json!({ "action": "cancel", "eventId": "evt_1", "title": TITLE, "startDate": DATE, "allDay": false });
    let items = h.process("cancelled", json!([cancel])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: cancelled"]);
    assert!(h.fake.names().is_empty());
    let items = h.process("moved", json!([cleaning("11:00")])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: cancelled or moved by a later email, not recreated"]
    );
    assert!(h.fake.names().is_empty());

    // Nor can the booking email, which wrote the event before "moved" did.
    let items = h.process("booking", json!([cleaning("11:00")])).await;
    assert_eq!(
        items,
        ["\"Cleaning\" on 2026-01-20: cancelled or moved by a later email, not recreated"]
    );
    assert!(h.fake.names().is_empty());

    // A new email booking the slot again is a real booking.
    let items = h.process("rebooked", json!([cleaning("11:00")])).await;
    assert_eq!(items, ["\"Cleaning\" on 2026-01-20: created"]);
    assert_eq!(h.fake.names(), [format!("{}.ics", uid("11:00"))]);
}

#[tokio::test]
async fn an_update_after_a_cancel_in_the_same_email_writes_nothing() {
    let h = harness().await;
    h.process("booking", json!([cleaning("09:00")])).await;
    let cancel = json!({ "action": "cancel", "eventId": "evt_1", "title": TITLE, "startDate": DATE, "allDay": false });
    let mut update = cleaning("10:00");
    update["action"] = json!("update");
    update["eventId"] = json!("evt_1");

    let items = h.process("confused", json!([cancel, update])).await;
    assert_eq!(
        items,
        [
            "\"Cleaning\" on 2026-01-20: cancelled",
            "\"Cleaning\" on 2026-01-20: event no longer tracked, skipped",
        ]
    );
    assert!(h.fake.names().is_empty());
    assert_eq!(h.count("PUT"), 1, "only the original create was written");
}

#[tokio::test]
async fn concurrent_runs_of_one_email_create_the_event_once() {
    let h = harness().await;
    h.app.ai.script(
        ModelRole::CalendarExtraction,
        vec![
            extraction(json!([cleaning("09:00")])),
            extraction(json!([cleaning("09:00")])),
        ],
    );
    let emails = [email(
        "notice-1",
        SENDER,
        "Your cleaning appointment",
        "See you then.",
    )];
    // The dispatcher and the retry task holding the same email.
    let (first, second) = tokio::join!(
        h.pipeline.handle_emails(&emails),
        h.pipeline.handle_emails(&emails)
    );
    first.unwrap();
    second.unwrap();

    let mut items: Vec<String> = h
        .support
        .activity()
        .iter()
        .flat_map(|a| a.items.clone().unwrap_or_default())
        .collect();
    items.sort();
    assert_eq!(
        items,
        [
            "\"Cleaning\" on 2026-01-20: created",
            "\"Cleaning\" on 2026-01-20: duplicate, skipped",
        ]
    );
    assert_eq!(h.count("PUT"), 1);
    assert_eq!(h.app.pushes.all().len(), 1);
    assert_eq!(h.fake.names().len(), 1);
}
