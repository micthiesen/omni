//! Calendar pipeline reliability (activity rows for `error`, `no_matches` and
//! the final outcome are written) and end-to-end create/cancel/update behavior.
//!
//! The real pipeline runs against a wiremock CalDAV server,
//! scripted models (`FakeModels` or a hanging model), a temp store and a
//! recording `EmailSupport`. "lost create acknowledgement" forces the record
//! write to fail with an SQLite trigger.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::{CALENDAR_URL, FakeSupport, caldav, email, extraction, pipeline};
use futures::future::BoxFuture;
use omni_ai::{
    AiError, GenerateRequest, GenerateResponse, LanguageModel, ModelId, ModelOverride, ModelRole,
};
use omni_calendar::persistence::{self, CreatedCalendarEvent, compute_calendar_event_uid};
use omni_calendar::support::{ActivityOutcome, AdmitTier};
use omni_store::StoreError;
use omni_testkit::{FakeFailure, TestApp};
use serde_json::json;
use tokio::sync::Notify;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KNOWN_SENDER: &str = "noreply@eventbrite.com";

fn appointment(id: &str) -> omni_core::email::FetchedEmail {
    email(id, KNOWN_SENDER, "Appointment", "Tomorrow")
}

#[tokio::test]
async fn durably_queues_admitted_email_when_calendar_discovery_fails() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(503).set_body_string("iCloud offline"))
        .mount(&server)
        .await;
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, None),
        support.clone(),
        app.ctx.ai.clone(),
    );

    p.handle_emails(&[appointment("mail-1")]).await.unwrap();

    let reason = "calendar discovery failed: CalDAV PROPFIND failed: 503 Service Unavailable (https://caldav.icloud.com/)\niCloud offline";
    assert_eq!(
        support.retries(),
        [(
            "CalendarEvents".to_owned(),
            "mail-1".to_owned(),
            reason.to_owned()
        )]
    );
    let activity = support.activity();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].outcome, ActivityOutcome::Error);
    assert_eq!(activity[0].detail.as_deref(), Some(reason));
    assert_eq!(activity[0].admit_tier, Some(AdmitTier::Builtin));
    assert_eq!(activity[0].cost_cents, None);
}

#[tokio::test(start_paused = true)]
async fn durably_queues_admitted_email_after_transient_extraction_failure() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    // The model client retries a 503 twice before giving up.
    for _ in 0..3 {
        app.ai.script_failure(
            ModelRole::CalendarExtraction,
            FakeFailure {
                status: 503,
                message: "model overloaded".to_owned(),
            },
        );
    }
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );

    p.handle_emails(&[appointment("mail-3")]).await.unwrap();

    assert_eq!(
        support.retries(),
        [(
            "CalendarEvents".to_owned(),
            "mail-3".to_owned(),
            "Calendar extraction failed: provider error 503: model overloaded".to_owned()
        )]
    );
    assert!(support.systemic().is_empty());
    // The error activity row is written.
    let activity = support.activity();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].outcome, ActivityOutcome::Error);
    assert_eq!(
        activity[0].detail.as_deref(),
        Some("extraction failed: Calendar extraction failed: provider error 503: model overloaded")
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn parks_admitted_email_for_a_new_build_after_a_rejected_extraction_request() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let message = "Invalid schema for response_format 'calendar_event_extraction': In context=('properties', 'events', 'items'), $ref cannot have keywords {'description'}.";
    app.ai.script_failure(
        ModelRole::CalendarExtraction,
        FakeFailure {
            status: 400,
            message: message.to_owned(),
        },
    );
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );

    p.handle_emails(&[appointment("mail-4")]).await.unwrap();

    // Not the retry schedule: the email waits for a build with the fix.
    assert!(support.retries().is_empty());
    let detail =
        format!("extraction failed: Calendar extraction failed: provider error 400: {message}");
    let signature = omni_email::systemic::classify_recorded(&detail).unwrap();
    assert_eq!(
        support.systemic(),
        [(
            "CalendarEvents".to_owned(),
            "mail-4".to_owned(),
            signature.key
        )]
    );
    let activity = support.activity();
    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].outcome, ActivityOutcome::Error);
    assert_eq!(activity[0].detail.as_deref(), Some(detail.as_str()));
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// A model whose call never completes; signals when it starts.
struct HangingModel {
    id: ModelId,
    started: Arc<Notify>,
}

impl LanguageModel for HangingModel {
    fn id(&self) -> &ModelId {
        &self.id
    }

    fn generate<'a>(
        &'a self,
        _req: &'a GenerateRequest,
    ) -> BoxFuture<'a, Result<GenerateResponse, AiError>> {
        self.started.notify_one();
        Box::pin(futures::future::pending())
    }
}

struct Hanging(Arc<Notify>);

impl ModelOverride for Hanging {
    fn model(&self, _role: Option<ModelRole>, id: &ModelId) -> Option<Arc<dyn LanguageModel>> {
        Some(Arc::new(HangingModel {
            id: id.clone(),
            started: self.0.clone(),
        }))
    }
}

#[tokio::test]
async fn preserves_interruption_while_event_extraction_is_in_progress() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let started = Arc::new(Notify::new());
    let ai = app
        .ctx
        .ai
        .clone()
        .with_override(Arc::new(Hanging(started.clone())));
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = Arc::new(pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        ai,
    ));

    let runner = p.clone();
    let task = tokio::spawn(async move {
        runner
            .handle_emails(&[appointment("mail-interrupted")])
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("extraction started");
    task.abort();
    let exit = task.await;

    assert!(exit.unwrap_err().is_cancelled());
    assert!(support.activity().is_empty());
    assert!(support.retries().is_empty());
}

#[tokio::test]
async fn replays_a_lost_create_acknowledgement_against_the_same_caldav_resource() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let event_hash = "dentist|2026-09-03|allday";
    let uid = compute_calendar_event_uid(event_hash);
    let event_path = format!("/123/calendars/home/{uid}.ics");
    Mock::given(method("PUT"))
        .and(path(event_path.as_str()))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(201))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(event_path.as_str()))
        .and(header("If-None-Match", "*"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let created = json!([{ "action": "create", "title": "Dentist", "startDate": "2026-09-03", "allDay": true }]);
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![extraction(created.clone()), extraction(created)],
    );
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );

    // Crash after CalDAV accepted the PUT: the record write fails.
    let record_pk = omni_store::entity::pk::<CreatedCalendarEvent>(&event_hash.to_owned()).unwrap();
    let trigger = format!(
        "CREATE TRIGGER fail_record BEFORE INSERT ON blobs WHEN NEW.pk = '{}' BEGIN SELECT RAISE(ABORT, 'crash after CalDAV accepted PUT'); END",
        record_pk.replace('\'', "''")
    );
    let exec = |sql: String| {
        let store = app.ctx.store.clone();
        async move {
            store
                .write(move |tx| {
                    tx.connection()
                        .execute_batch(&sql)
                        .map_err(|e| StoreError::Sqlite(e.to_string()))
                })
                .await
                .unwrap();
        }
    };
    exec(trigger).await;
    let first = p.handle_emails(&[appointment("mail-replay")]).await;
    assert!(
        first
            .unwrap_err()
            .to_string()
            .contains("crash after CalDAV accepted PUT")
    );
    exec("DROP TRIGGER fail_record".to_owned()).await;

    p.handle_emails(&[appointment("mail-replay")])
        .await
        .unwrap();

    let puts: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(puts, [event_path.clone(), event_path]);
    let stored = persistence::get_tracked_event(&app.ctx.store, event_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.calendar_event_id, uid);
    // The replay reconciled (412) and still records and notifies once.
    assert_eq!(app.pushes.all().len(), 1);
}

#[tokio::test]
async fn records_error_no_matches_and_final_outcome_activity_rows() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    app.ai.script_failure(
        ModelRole::CalendarExtraction,
        FakeFailure {
            status: 400,
            message: "bad request".to_owned(),
        },
    );
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![
            extraction(json!([])),
            extraction(json!([
                { "action": "create", "title": "🦷 Dentist", "startDate": "2026-09-03", "startTime": "09:00", "allDay": false, "location": "Clinic" },
                { "action": "cancel", "eventId": null, "title": "Old thing", "startDate": "2026-09-04", "allDay": true }
            ])),
        ],
    );
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    *support.triage_cost.lock().unwrap() = Some(0.25);
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );

    p.handle_emails(&[
        email("e-error", "clinic@example.com", "Appointment", "x"),
        email("e-none", "clinic@example.com", "Newsletter", "x"),
        email("e-items", "clinic@example.com", "Booking", "x"),
        email("e-filtered", "news@example.com", "Weekly", "x"),
    ])
    .await
    .unwrap();

    let activity = support.activity();
    let outcomes: Vec<(&str, ActivityOutcome)> = activity
        .iter()
        .map(|a| (a.email_id.as_str(), a.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("e-filtered", ActivityOutcome::Filtered),
            ("e-error", ActivityOutcome::Error),
            ("e-none", ActivityOutcome::NoMatches),
            ("e-items", ActivityOutcome::Partial),
        ]
    );
    let filtered = &activity[0];
    assert_eq!(filtered.detail.as_deref(), Some("blacklisted sender"));
    assert_eq!(filtered.cost_cents, Some(Some(0.25)));
    let none = &activity[2];
    assert_eq!(none.detail.as_deref(), Some("no calendar events found"));
    assert_eq!(none.admit_tier, Some(AdmitTier::Triage));
    assert_eq!(
        none.admit_reason.as_deref(),
        Some("triage: upcoming appointment")
    );
    let items = &activity[3];
    assert_eq!(
        items.items.as_deref().unwrap(),
        [
            "\"🦷 Dentist\" on 2026-09-03: created",
            "\"Old thing\": cancel without explicit reference, skipped",
        ]
    );
    // Captures wrap only the processing phase of admitted emails.
    assert_eq!(
        support.recorded.lock().unwrap().captures,
        [
            "CalendarEvents#e-error",
            "CalendarEvents#e-none",
            "CalendarEvents#e-items"
        ]
    );
    let pushes = app.pushes.all();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0].message.title.as_deref(),
        Some("Calendar Event Created")
    );
    assert_eq!(
        pushes[0].message.message,
        "🦷 Dentist\n2026-09-03 at 09:00\nClinic"
    );
}

#[tokio::test]
async fn cancels_only_through_an_explicit_handle_and_updates_with_backfill() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    // Two tracked events inside the prompt window (test clock is 2026-01-01).
    let mut haircut = CreatedCalendarEvent::from_event(
        "haircut|2026-01-20|10:00".to_owned(),
        "e0".to_owned(),
        "omni-haircut@omni-notify".to_owned(),
        &omni_calendar::extraction::schema::ExtractedEvent::new(
            omni_calendar::extraction::schema::EventAction::Create,
            "💇 Haircut",
            "2026-01-20",
            false,
        ),
        1,
    );
    haircut.start_time = Some("10:00".to_owned());
    haircut.description = Some("Ask for Sam".to_owned());
    haircut.reminder_minutes = Some(60.0);
    // The calendar copy carries a manual edit the update must keep.
    let copy = omni_calendar::caldav::ics::build_icalendar(
        &haircut.to_event(),
        "omni-haircut@omni-notify",
        omni_testkit::TEST_EPOCH_MS,
        "America/Vancouver",
    )
    .replace("END:VEVENT", "LOCATION:Salon on Main\r\nEND:VEVENT");
    Mock::given(method("GET"))
        .and(path("/123/calendars/home/omni-haircut@omni-notify.ics"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"h1\"")
                .set_body_string(copy),
        )
        .mount(&server)
        .await;
    persistence::record_created_event(&app.ctx.store, haircut)
        .await
        .unwrap();
    let dentist = CreatedCalendarEvent::from_event(
        "dentist|2026-01-25|allday".to_owned(),
        "e0".to_owned(),
        "omni-dentist@omni-notify".to_owned(),
        &omni_calendar::extraction::schema::ExtractedEvent::new(
            omni_calendar::extraction::schema::EventAction::Create,
            "🦷 Dentist",
            "2026-01-25",
            true,
        ),
        1,
    );
    persistence::record_created_event(&app.ctx.store, dentist)
        .await
        .unwrap();

    // Handles follow store (insertion) order: evt_1 = haircut, evt_2 = dentist.
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![extraction(json!([
            { "action": "update", "eventId": "[evt_1]", "title": "💇 Haircut", "startDate": "2026-01-21", "startTime": "11:00", "allDay": false },
            { "action": "cancel", "eventId": "evt_2", "title": "🦷 Dentist", "startDate": "2026-01-25", "allDay": true }
        ]))],
    );
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );
    p.handle_emails(&[appointment("e-change")]).await.unwrap();

    let prompt = &app.ai.requests()[0].1;
    let omni_ai::ContentPart::Text { text } = &prompt.messages[0].content[0] else {
        panic!("text prompt expected");
    };
    assert!(
        text.contains("- [evt_2] \"🦷 Dentist\" on 2026-01-25 (all day)"),
        "{text}"
    );
    assert!(
        text.contains("- [evt_1] \"💇 Haircut\" on 2026-01-20 at 10:00"),
        "{text}"
    );

    let activity = support.activity();
    assert_eq!(activity[0].outcome, ActivityOutcome::Processed);
    assert_eq!(
        activity[0].items.as_deref().unwrap(),
        [
            "\"💇 Haircut\" on 2026-01-21: updated",
            "\"🦷 Dentist\" on 2026-01-25: cancelled"
        ]
    );
    let all = persistence::get_tracked_events(&app.ctx.store)
        .await
        .unwrap();
    let moved = all
        .iter()
        .find(|e| e.event_hash == "haircut|2026-01-21|11:00")
        .unwrap();
    assert_eq!(moved.calendar_event_id, "omni-haircut@omni-notify");
    // Fields the model cannot see were backfilled from the stored record.
    assert_eq!(moved.description.as_deref(), Some("Ask for Sam"));
    assert_eq!(moved.reminder_minutes, Some(60.0));
    assert!(
        all.iter()
            .all(|e| e.event_hash == moved.event_hash || e.is_cancelled())
    );

    let requests = server.received_requests().await.unwrap();
    let calls: Vec<String> = requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect();
    assert_eq!(
        calls,
        [
            "GET /123/calendars/home/omni-haircut@omni-notify.ics",
            "PUT /123/calendars/home/omni-haircut@omni-notify.ics",
            "DELETE /123/calendars/home/omni-dentist@omni-notify.ics"
        ]
    );
    let put = &requests[1];
    assert_eq!(put.headers.get("if-match").unwrap(), "\"h1\"");
    let body = String::from_utf8_lossy(&put.body);
    assert!(body.contains("DESCRIPTION:Ask for Sam\r\n"), "{body}");
    assert!(body.contains("TRIGGER:-PT1H\r\n"), "{body}");
    assert!(body.contains("T110000\r\n"), "{body}");
    assert!(
        body.contains("LOCATION:Salon on Main\r\n"),
        "manual edit kept: {body}"
    );
}

#[tokio::test]
async fn server_errors_queue_a_retry_and_client_errors_do_not() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!(
            "/123/calendars/home/{}.ics",
            compute_calendar_event_uid("a|2026-02-01|allday")
        )))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![extraction(json!([
            { "action": "create", "title": "A", "startDate": "2026-02-01", "allDay": true },
            { "action": "create", "title": "B", "startDate": "2026-02-02", "allDay": true }
        ]))],
    );
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let p = pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        support.clone(),
        app.ctx.ai.clone(),
    );
    p.handle_emails(&[appointment("e-fail")]).await.unwrap();

    assert_eq!(
        support.retries(),
        [(
            "CalendarEvents".to_owned(),
            "e-fail".to_owned(),
            "CalDAV 502: Bad Gateway".to_owned()
        )]
    );
    let activity = support.activity();
    assert_eq!(activity[0].outcome, ActivityOutcome::Failed);
    assert!(app.pushes.all().is_empty());
    assert!(
        persistence::get_tracked_events(&app.ctx.store)
            .await
            .unwrap()
            .is_empty()
    );
}
