//! The calendar pipeline over the real `omni_email` core
//! ([`OmniEmailSupport`]): sender rules, shared triage, activity rows, the
//! durable retry queue and per-email log capture all land in the store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, OnceLock};

use common::{CALENDAR_URL, NoAttachments, caldav, email, extraction};
use futures::future::BoxFuture;
use omni_ai::ModelRole;
use omni_calendar::pipeline::{CalendarEventPipeline, PipelineDeps};
use omni_calendar::{Caldav, OmniEmailSupport};
use omni_core::clock::SystemClock;
use omni_email::activity::{self, AdmitTier, EmailActivityOutcome, LlmCost};
use omni_email::sender_rules::{self, RuleScope, RuleVerdict};
use omni_email::triage::{
    Classified, EmailTriage, TriageClassifier, TriageEmail, TriageError, TriageVerdict,
};
use omni_email::{activity_logs, retry};
use omni_tasks::{EventBus, RunLogLayer, RunLogs};
use omni_testkit::TestApp;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Says "calendar" for every email, at a fixed price.
struct CalendarClassifier;

impl TriageClassifier for CalendarClassifier {
    fn classify(&self, _email: TriageEmail) -> BoxFuture<'static, Result<Classified, TriageError>> {
        Box::pin(async {
            Ok(Classified {
                verdict: TriageVerdict {
                    parcel: false,
                    calendar: true,
                    reason: "an appointment".to_owned(),
                },
                cost: Some(LlmCost::Cents(0.5)),
            })
        })
    }
}

fn pipeline(app: &TestApp, caldav: Caldav, run_logs: RunLogs) -> CalendarEventPipeline {
    CalendarEventPipeline::new(PipelineDeps {
        config: app.ctx.config.clone(),
        store: app.ctx.store.clone(),
        clock: app.ctx.clock.clone(),
        ai: app.ctx.ai.clone(),
        pushover: app.ctx.pushover.clone(),
        caldav,
        support: Arc::new(OmniEmailSupport::new(
            app.ctx.store.clone(),
            run_logs,
            EmailTriage::new(Arc::new(CalendarClassifier)),
            app.ctx.pushover.clone(),
        )),
        attachments: Arc::new(NoAttachments),
    })
}

/// One process-wide capture subscriber: callsite interest is global, so a
/// per-test scoped subscriber would race with tests on other threads.
fn run_logs() -> RunLogs {
    static LOGS: OnceLock<RunLogs> = OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = RunLogs::new(EventBus::new(16), Arc::new(SystemClock));
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(RunLogLayer::new(logs.clone())),
        )
        .expect("no other global subscriber");
        logs
    })
    .clone()
}

#[tokio::test]
async fn sender_rules_are_read_from_the_email_core_for_the_calendar_scope() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    let store = &app.ctx.store;
    sender_rules::upsert(
        store,
        "@blocked.example",
        RuleScope::Calendar,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    // A parcel-only block does not apply to the calendar pipeline.
    sender_rules::upsert(
        store,
        "@parcel-only.example",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    // An explicit allow beats the built-in blacklist (`news@`).
    sender_rules::upsert(
        store,
        "news@allowed.example",
        RuleScope::Both,
        RuleVerdict::Allow,
    )
    .await
    .unwrap();
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![extraction(json!([])), extraction(json!([]))],
    );
    let p = pipeline(&app, caldav(&app, &server, Some(CALENDAR_URL)), run_logs());

    p.handle_emails(&[
        email("r-block", "clinic@blocked.example", "Appointment", "x"),
        email("r-parcel", "shop@parcel-only.example", "Appointment", "x"),
        email("r-allow", "news@allowed.example", "Appointment", "x"),
    ])
    .await
    .unwrap();

    let blocked = activity::get(store, "CalendarEvents#r-block")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked.outcome, EmailActivityOutcome::Filtered);
    assert_eq!(
        blocked.detail.as_deref(),
        Some("blocked by rule @blocked.example")
    );
    // No triage call ran for this email, so its cost reads as unpriced (null).
    assert_eq!(blocked.cost_cents, LlmCost::Unpriced);

    let parcel = activity::get(store, "CalendarEvents#r-parcel")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parcel.outcome, EmailActivityOutcome::NoMatches);
    assert_eq!(parcel.admit_tier, Some(AdmitTier::Triage));
    assert_eq!(
        parcel.admit_reason.as_deref(),
        Some("triage: an appointment")
    );

    let allowed = activity::get(store, "CalendarEvents#r-allow")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(allowed.outcome, EmailActivityOutcome::NoMatches);
    assert_eq!(allowed.admit_tier, Some(AdmitTier::Rule));
    assert_eq!(
        allowed.admit_reason.as_deref(),
        Some("allowed by rule news@allowed.example")
    );
    // Rule admission never attributes the (0.5 cent) triage cost.
    assert!(
        !matches!(allowed.cost_cents, LlmCost::Cents(c) if c >= 0.5),
        "{:?}",
        allowed.cost_cents
    );
}

#[tokio::test]
async fn discovery_failure_is_queued_in_the_durable_retry_queue() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let p = pipeline(&app, caldav(&app, &server, None), run_logs());

    p.handle_emails(&[email("d-1", "clinic@example.com", "Appointment", "x")])
        .await
        .unwrap();

    let reason = "calendar discovery failed: CalDAV PROPFIND failed: 503 Service Unavailable (https://caldav.icloud.com/)\n";
    let queued = retry::get(&app.ctx.store, "CalendarEvents#d-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(queued.pipeline, "CalendarEvents");
    assert_eq!(queued.reason, reason);
    let row = activity::get(&app.ctx.store, "CalendarEvents#d-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, EmailActivityOutcome::Error);
    assert_eq!(row.detail.as_deref(), Some(reason));
    assert_eq!(row.cost_cents, LlmCost::Cents(0.5));
}

#[tokio::test]
async fn processed_emails_record_activity_and_their_captured_log() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    app.ai.script(
        ModelRole::CalendarExtraction,
        vec![extraction(json!([
            { "action": "create", "title": "🦷 Dentist", "startDate": "2026-09-03", "startTime": "09:00", "allDay": false }
        ]))],
    );
    let p = pipeline(&app, caldav(&app, &server, Some(CALENDAR_URL)), run_logs());

    p.handle_emails(&[email("p-1", "clinic@example.com", "Appointment", "x")])
        .await
        .unwrap();

    let row = activity::get(&app.ctx.store, "CalendarEvents#p-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.outcome, EmailActivityOutcome::Processed);
    assert_eq!(
        row.items.as_deref().unwrap(),
        ["\"🦷 Dentist\" on 2026-09-03: created"]
    );
    assert_eq!(row.received_at, 1_788_220_800_000);
    let captured = activity_logs::get(&app.ctx.store, "CalendarEvents#p-1")
        .await
        .unwrap()
        .expect("captured log row");
    let messages: Vec<&str> = captured.lines.iter().map(|l| l.msg.as_str()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("Extracting events from: \"Appointment\"")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("Created: \"🦷 Dentist\"")),
        "{messages:?}"
    );
    assert!(retry::get_all(&app.ctx.store).await.unwrap().is_empty());
}

/// Answers `fetch_by_id` with one fixed email.
struct OneEmail(omni_core::email::FetchedEmail);

impl omni_runtime::ports::EmailReader for OneEmail {
    fn fetch_by_id<'a>(
        &'a self,
        id: &'a str,
        _fresh: bool,
    ) -> BoxFuture<'a, Result<Option<omni_core::email::FetchedEmail>, omni_runtime::ports::PortError>>
    {
        let found = (id == self.0.id).then(|| self.0.clone());
        Box::pin(async move { Ok(found) })
    }

    fn search<'a>(
        &'a self,
        _q: &'a omni_runtime::ports::EmailSearch,
    ) -> BoxFuture<'a, Result<Vec<omni_core::email::FetchedEmail>, omni_runtime::ports::PortError>>
    {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn health(&self) -> omni_runtime::ports::EmailReaderHealth {
        omni_runtime::ports::EmailReaderHealth {
            transport: "IMAP".to_owned(),
            search_available: false,
            drafts_available: false,
        }
    }

    fn download_attachment<'a>(
        &'a self,
        _attachment: &'a omni_core::email::EmailAttachment,
    ) -> BoxFuture<
        'a,
        Result<Option<omni_core::email::DownloadedAttachment>, omni_runtime::ports::PortError>,
    > {
        Box::pin(async { Ok(None) })
    }
}

struct OnlyCalendar(Arc<CalendarEventPipeline>);

impl omni_runtime::ports::EmailRetryHandlers for OnlyCalendar {
    fn handler(&self, pipeline: &str) -> Option<Arc<dyn omni_core::email::EmailHandler>> {
        (pipeline == "CalendarEvents")
            .then(|| self.0.clone() as Arc<dyn omni_core::email::EmailHandler>)
    }
}

#[tokio::test]
async fn a_rejected_extraction_request_replays_once_a_new_build_runs() {
    let app = TestApp::new().await;
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    app.ai.script_failure(
        ModelRole::CalendarExtraction,
        omni_testkit::FakeFailure {
            status: 400,
            message: "Invalid schema for response_format 'calendar_event_extraction': $ref cannot have keywords {'description'}.".to_owned(),
        },
    );
    let create = || {
        extraction(json!([
            { "action": "create", "title": "Strata AGM", "startDate": "2026-11-03", "startTime": "18:00", "allDay": false }
        ]))
    };
    app.ai
        .script(ModelRole::CalendarExtraction, vec![create(), create()]);
    let mail = email("bcs-1", "strata@example.com", "AGM notice", "x");
    let p = Arc::new(pipeline(
        &app,
        caldav(&app, &server, Some(CALENDAR_URL)),
        run_logs(),
    ));
    let store = &app.ctx.store;

    p.handle_emails(std::slice::from_ref(&mail)).await.unwrap();

    let parked = retry::get(store, "CalendarEvents#bcs-1")
        .await
        .unwrap()
        .unwrap();
    let build = omni_email::systemic::current_build().await;
    assert_eq!(parked.awaiting_build.as_deref(), Some(build.as_str()));
    assert_eq!(app.pushes.all().len(), 1);
    let failed = activity::get(store, "CalendarEvents#bcs-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.outcome, EmailActivityOutcome::Error);

    let ports = omni_runtime::Ports::default();
    ports
        .set_email_reader(Arc::new(OneEmail(mail.clone())))
        .ok()
        .unwrap();
    ports
        .set_email_retry_handlers(Arc::new(OnlyCalendar(p.clone())))
        .ok()
        .unwrap();
    let task = omni_email::retry_task::EmailRetryTask::new(
        store.clone(),
        ports,
        omni_tasks::CronSchedule::parse(omni_email::retry_task::SCHEDULE, &jiff::tz::TimeZone::UTC)
            .unwrap(),
    );
    // Same build: nothing replays.
    task.run_pass().await.unwrap();
    assert!(
        retry::get(store, "CalendarEvents#bcs-1")
            .await
            .unwrap()
            .is_some()
    );

    let report = omni_email::systemic::release_for_build(store, "sha256:nextbuild", 20)
        .await
        .unwrap();
    assert_eq!(report.released, 1);
    task.run_pass().await.unwrap();

    assert!(retry::get_all(store).await.unwrap().is_empty());
    let replayed = activity::get(store, "CalendarEvents#bcs-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replayed.outcome, EmailActivityOutcome::Processed);
    assert_eq!(replayed.detail.as_deref(), Some("replayed after fix"));

    // Handler dedup: processing the same email again creates nothing new.
    p.handle_emails(std::slice::from_ref(&mail)).await.unwrap();
    let again = activity::get(store, "CalendarEvents#bcs-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        again.items.as_deref().unwrap(),
        ["\"Strata AGM\" on 2026-11-03: duplicate, skipped"]
    );
    server.verify().await;
}
