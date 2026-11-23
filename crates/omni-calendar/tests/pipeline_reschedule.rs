//! An email that reschedules an already tracked event updates that event
//! instead of creating a second one (the production duplicates: a vet
//! follow-up created at 16:30 and again at 17:00 on the same day, and a strata
//! AGM created on 11-08 and again on 11-25).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{CALENDAR_URL, FakeSupport, caldav, email, extraction, pipeline};
use omni_ai::ModelRole;
use omni_calendar::extraction::schema::{EventAction, ExtractedEvent};
use omni_calendar::persistence::{
    self, CreatedCalendarEvent, compute_event_hash, find_reschedule_target, mentions_reschedule,
};
use omni_testkit::TestApp;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SENDER: &str = "noreply@eventbrite.com";

fn tracked(title: &str, date: &str, time: &str, uid: &str) -> CreatedCalendarEvent {
    let mut event = ExtractedEvent::new(EventAction::Create, title, date, false);
    event.start_time = Some(time.to_owned());
    CreatedCalendarEvent::from_event(
        compute_event_hash(title, date, Some(time)),
        "first-email".to_owned(),
        uid.to_owned(),
        &event,
        1,
    )
}

async fn serve_copy(server: &MockServer, row: &CreatedCalendarEvent) {
    let body = omni_calendar::caldav::ics::build_icalendar(
        &row.to_event(),
        &row.calendar_event_id,
        omni_testkit::TEST_EPOCH_MS,
        "America/Vancouver",
    );
    Mock::given(method("GET"))
        .and(path(format!(
            "/123/calendars/home/{}.ics",
            row.calendar_event_id
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"v1\"")
                .set_body_string(body),
        )
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(server)
        .await;
}

async fn run(
    app: &TestApp,
    server: &MockServer,
    subject: &str,
    body: &str,
    events: serde_json::Value,
) -> Vec<String> {
    app.ai
        .script(ModelRole::CalendarExtraction, vec![extraction(events)]);
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        app,
        caldav(app, server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );
    p.handle_emails(&[email("second-email", SENDER, subject, body)])
        .await
        .unwrap();
    support.activity()[0].items.clone().unwrap_or_default()
}

fn calls(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

#[tokio::test]
async fn a_new_time_on_the_same_day_updates_the_tracked_event() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let first = tracked(
        "🐾 Sam's Post-I-131 Veterinary Follow-Up",
        "2026-10-02",
        "16:30",
        "omni-vet@omni-notify",
    );
    serve_copy(&server, &first).await;
    persistence::record_created_event(&app.ctx.store, first)
        .await
        .unwrap();

    let items = run(
        &app,
        &server,
        "Appointment confirmation",
        "Sam's follow-up is booked for October 2 at 5:00 PM.",
        json!([{ "action": "create", "title": "🐶 Sam's Post-I-131 Veterinary Follow-Up", "startDate": "2026-10-02", "startTime": "17:00", "allDay": false }]),
    )
    .await;
    assert_eq!(
        items,
        ["\"🐶 Sam's Post-I-131 Veterinary Follow-Up\" on 2026-10-02: updated"]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        calls(&requests),
        [
            "GET /123/calendars/home/omni-vet@omni-notify.ics",
            "PUT /123/calendars/home/omni-vet@omni-notify.ics",
        ]
    );
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(
        body.contains("DTSTART;TZID=America/Vancouver:20261002T170000\r\n"),
        "{body}"
    );
    let active: Vec<_> = persistence::get_tracked_events(&app.ctx.store)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_cancelled())
        .collect();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].start_time.as_deref(), Some("17:00"));
    assert_eq!(active[0].calendar_event_id, "omni-vet@omni-notify");
}

#[tokio::test]
async fn a_rescheduled_date_updates_the_tracked_event() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let first = tracked(
        "🏢 Uno Strata Annual General Meeting",
        "2026-11-08",
        "19:00",
        "omni-agm@omni-notify",
    );
    serve_copy(&server, &first).await;
    persistence::record_created_event(&app.ctx.store, first)
        .await
        .unwrap();

    let items = run(
        &app,
        &server,
        "Uno Strata AGM - new date",
        "The Annual General Meeting has been rescheduled to Wednesday, November 25 at 7 PM.",
        json!([{ "action": "create", "title": "🏢 Uno Strata Annual General Meeting", "startDate": "2026-11-25", "startTime": "19:00", "allDay": false }]),
    )
    .await;
    assert_eq!(
        items,
        ["\"🏢 Uno Strata Annual General Meeting\" on 2026-11-25: updated"]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        calls(&requests)[1],
        "PUT /123/calendars/home/omni-agm@omni-notify.ics"
    );
    let body = String::from_utf8_lossy(&requests[1].body);
    assert!(body.contains(":20261125T190000\r\n"), "{body}");
}

#[tokio::test]
async fn another_date_without_reschedule_language_is_a_separate_event() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let first = tracked("Book Club", "2026-11-08", "19:00", "omni-club@omni-notify");
    serve_copy(&server, &first).await;
    persistence::record_created_event(&app.ctx.store, first)
        .await
        .unwrap();
    let items = run(
        &app,
        &server,
        "Book Club",
        "Our next meeting is on November 22.",
        json!([{ "action": "create", "title": "Book Club", "startDate": "2026-11-22", "startTime": "19:00", "allDay": false }]),
    )
    .await;
    assert_eq!(items, ["\"Book Club\" on 2026-11-22: created"]);
    let active = persistence::get_tracked_events(&app.ctx.store)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_cancelled())
        .count();
    assert_eq!(active, 2);
}

#[tokio::test]
async fn an_email_listing_two_sessions_keeps_the_tracked_one() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let first = tracked(
        "Swim Lesson",
        "2026-10-10",
        "09:00",
        "omni-swim@omni-notify",
    );
    serve_copy(&server, &first).await;
    persistence::record_created_event(&app.ctx.store, first)
        .await
        .unwrap();
    let items = run(
        &app,
        &server,
        "Your bookings",
        "Swim lessons on October 10 at 9:00 AM and 3:00 PM.",
        json!([
            { "action": "create", "title": "Swim Lesson", "startDate": "2026-10-10", "startTime": "09:00", "allDay": false },
            { "action": "create", "title": "Swim Lesson", "startDate": "2026-10-10", "startTime": "15:00", "allDay": false },
        ]),
    )
    .await;
    assert_eq!(
        items,
        [
            "\"Swim Lesson\" on 2026-10-10: duplicate, skipped",
            "\"Swim Lesson\" on 2026-10-10: created",
        ]
    );
    let requests = server.received_requests().await.unwrap();
    assert!(
        calls(&requests)
            .iter()
            .all(|c| !c.contains("omni-swim@omni-notify")),
        "the tracked 09:00 lesson must not be moved: {:?}",
        calls(&requests)
    );
    let active = persistence::get_tracked_events(&app.ctx.store)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_cancelled())
        .count();
    assert_eq!(active, 2);
}

#[test]
fn matching_rules() {
    let rows = [
        tracked("Vet", "2026-10-02", "16:30", "a"),
        tracked("Vet", "2026-10-02", "09:00", "b"),
        tracked("AGM", "2026-11-08", "19:00", "c"),
    ];
    let mut vet = ExtractedEvent::new(EventAction::Create, "Vet", "2026-10-02", false);
    vet.start_time = Some("17:00".to_owned());
    // Two same-day candidates: ambiguous, so it creates.
    assert!(find_reschedule_target(&vet, rows.iter(), false).is_none());
    let mut agm = ExtractedEvent::new(EventAction::Create, "AGM", "2026-11-25", false);
    agm.start_time = Some("19:00".to_owned());
    assert!(find_reschedule_target(&agm, rows.iter(), false).is_none());
    assert_eq!(
        find_reschedule_target(&agm, rows.iter(), true).map(|r| r.calendar_event_id.as_str()),
        Some("c")
    );
    let far = ExtractedEvent::new(EventAction::Create, "AGM", "2027-03-01", false);
    assert!(find_reschedule_target(&far, rows.iter(), true).is_none());
    // The exact same event is a duplicate, not a reschedule.
    let mut same = ExtractedEvent::new(EventAction::Create, "AGM", "2026-11-08", false);
    same.start_time = Some("19:00".to_owned());
    assert!(find_reschedule_target(&same, rows.iter(), true).is_none());

    assert!(mentions_reschedule("Your appointment has been RESCHEDULED"));
    assert!(mentions_reschedule("The meeting is postponed"));
    assert!(mentions_reschedule("New date: November 25"));
    assert!(!mentions_reschedule("See you on November 22"));
}
