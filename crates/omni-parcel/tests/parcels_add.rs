//! `parcels_add` through its MCP handler (input and output schemas included)
//! against a local Parcel mock: validation, deduplication against email
//! submissions, the shared daily add limit, Parcel's answers, and the email
//! pipeline submitting through the same client with the sync gone.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use omni_core::clock::TestClock;
use omni_email::systemic::SystemicReporter;
use omni_email::triage::EmailTriage;
use omni_http::SideEffectMode;
use omni_http::public::PublicHttpClient;
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput, ToolPhase};
use omni_parcel::add::ParcelAdder;
use omni_parcel::add_ledger::{self, AddLedger, TOOL_CEILING, WINDOW_MS};
use omni_parcel::carriers::carrier_map::CarrierDirectory;
use omni_parcel::parcel_api::ParcelApi;
use omni_parcel::persistence::{
    self, DeliveryAttempt, SubmissionOrigin, SubmissionStatus, SubmittedDelivery,
};
use omni_parcel::pipeline::{DeliveryPipeline, PipelineDeps};
use omni_store::Store;
use omni_store::entity::EntityOps as _;
use omni_tasks::{EventBus, RunLogs};
use omni_testkit::TestStore;
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{always_extract, deliveries, shipment};

const NOW: i64 = common::NOW;
const ADD_PATH: &str = "/external/add-delivery/";
const UNKNOWN_FORMAT: &str =
    "The tracking number submitted does not match any previously known formats";
const ALREADY_ADDED: &str = "You have already added this delivery to the app";

fn carriers() -> Value {
    json!({
        "ups": {"name": "UPS"},
        "cp": {"name": "Canada Post"},
        "puro": {"name": "Purolator"},
        "amzlca": {"name": "Amazon Canada"},
    })
}

struct Fixture {
    store: TestStore,
    clock: Arc<TestClock>,
    server: MockServer,
    tool: McpTool,
}

async fn fixture_with(mode: SideEffectMode, configured: bool) -> Fixture {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/external/supported_carriers.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(carriers()))
        .mount(&server)
        .await;
    let clock = TestClock::new(NOW);
    let store = TestStore::new(clock.clone()).await;
    let http = omni_testkit::mock_http(&server, &["https://api.parcel.app"]);
    let adder = if configured {
        let api = ParcelApi::new(http.clone(), "parcel-key".to_owned(), mode, clock.clone())
            .unwrap()
            .with_ledger(store.store.clone());
        let directory = CarrierDirectory::new(
            PublicHttpClient::new(&http).allow_loopback_for_tests(),
            clock.clone(),
        )
        .unwrap();
        ParcelAdder::new(
            store.store.clone(),
            Some(Arc::new(api)),
            Some(Arc::new(directory)),
            TaskTracker::new(),
        )
    } else {
        ParcelAdder::new(store.store.clone(), None, None, TaskTracker::new())
    };
    let tool = omni_parcel::mcp::tools(Arc::new(adder)).unwrap().remove(0);
    Fixture {
        store,
        clock,
        server,
        tool,
    }
}

async fn fixture() -> Fixture {
    fixture_with(SideEffectMode::Live, true).await
}

async fn parcel_answers(server: &MockServer, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path(ADD_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn call(tool: &McpTool, input: Value) -> Result<Value, omni_mcp_kit::ToolError> {
    match tool.handler.call(input, standalone_context("t")).await? {
        ToolOutput::Structured(map) => Ok(Value::Object(map)),
        ToolOutput::Custom { structured, .. } => Ok(Value::Object(structured)),
    }
}

async fn add_requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == ADD_PATH)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// Real time passes while the mock answers over a socket, so "now" readings
/// are checked within a bound.
const SLACK_MS: i64 = 10_000;

fn near(actual: i64, expected: i64) -> bool {
    (expected..expected + SLACK_MS).contains(&actual)
}

fn iso_ms(value: &Value) -> i64 {
    value
        .as_str()
        .unwrap()
        .parse::<jiff::Timestamp>()
        .unwrap()
        .as_millisecond()
}

async fn row(store: &Store, tracking_number: &str) -> Option<SubmittedDelivery> {
    persistence::get(store, tracking_number).await.unwrap()
}

async fn ledger(store: &Store) -> Option<AddLedger> {
    store
        .read(|docs| docs.get::<AddLedger>(&"parcel".to_owned()))
        .await
        .unwrap()
}

async fn email_submission(store: &Store, tracking_number: &str, status: SubmissionStatus) {
    persistence::record(
        store,
        DeliveryAttempt {
            tracking_number: tracking_number.to_owned(),
            carrier_code: "ups".to_owned(),
            description: "Camera".to_owned(),
            submitted_at: NOW - 1_000,
            email_id: "mail-1".to_owned(),
        },
        status,
        Some(1),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn submits_a_normalized_delivery_and_records_it_for_dedup() {
    let f = fixture().await;
    parcel_answers(&f.server, 200, json!({"success": true})).await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": " 1Z999AA1 0123 456784 ", "carrier": " UPS ", "description": "Uniqlo order"}),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        json!({
            "status": "accepted", "trackingNumber": "1Z999AA10123456784", "carrier": "ups",
            "carrierName": "UPS", "description": "Uniqlo order",
            "message": "Parcel accepted the delivery."
        })
    );
    assert_eq!(
        add_requests(&f.server).await,
        [json!({
            "tracking_number": "1Z999AA10123456784", "carrier_code": "ups",
            "description": "Uniqlo order", "send_push_confirmation": false
        })]
    );
    let stored = row(&f.store.store, "1Z999AA10123456784").await.unwrap();
    assert_eq!(stored.status, Some(SubmissionStatus::Submitted));
    assert_eq!(stored.origin, Some(SubmissionOrigin::ParcelsAdd));
    assert_eq!(stored.email_id, "");
    let attempts = ledger(&f.store.store).await.unwrap().attempts;
    assert_eq!(attempts.len(), 1);
    assert!(near(attempts[0], NOW));
}

#[tokio::test]
async fn defaults_the_description_and_forwards_the_push_choice() {
    let f = fixture().await;
    parcel_answers(&f.server, 200, json!({"success": true})).await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "1234567890123", "carrier": "cp", "description": "  ", "sendPushConfirmation": true}),
    )
    .await
    .unwrap();
    assert_eq!(out["description"], "Canada Post package");
    assert_eq!(
        add_requests(&f.server).await[0]["send_push_confirmation"],
        true
    );
}

#[tokio::test]
async fn reports_numbers_the_email_pipeline_already_submitted_without_calling_parcel() {
    let f = fixture().await;
    email_submission(
        &f.store.store,
        "1Z999AA10123456784",
        SubmissionStatus::Submitted,
    )
    .await;
    email_submission(&f.store.store, "P5253806501", SubmissionStatus::Submitted).await;

    let exact = call(
        &f.tool,
        json!({"trackingNumber": "1z999aa1 0123456784", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(exact["status"], "duplicate");
    assert_eq!(exact["duplicateOf"], "1Z999AA10123456784");
    assert!(
        exact["message"]
            .as_str()
            .unwrap()
            .contains("the email pipeline")
    );

    // Merchants truncate the same number differently.
    let near = call(
        &f.tool,
        json!({"trackingNumber": "P52538065", "carrier": "cp"}),
    )
    .await
    .unwrap();
    assert_eq!(near["status"], "duplicate");
    assert_eq!(near["duplicateOf"], "P5253806501");

    assert!(add_requests(&f.server).await.is_empty());
    assert!(ledger(&f.store.store).await.is_none());
}

#[tokio::test]
async fn retries_a_number_parcel_rejected_from_email_and_restores_it_on_refusal() {
    let f = fixture().await;
    email_submission(&f.store.store, "1234567890123", SubmissionStatus::Rejected).await;
    parcel_answers(
        &f.server,
        400,
        json!({"success": false, "error_message": UNKNOWN_FORMAT}),
    )
    .await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "1234567890123", "carrier": "puro"}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "error");
    assert_eq!(out["errorKind"], "rejected");
    assert_eq!(out["parcelMessage"], UNKNOWN_FORMAT);
    assert_eq!(add_requests(&f.server).await.len(), 1);
    let restored = row(&f.store.store, "1234567890123").await.unwrap();
    assert_eq!(restored.status, Some(SubmissionStatus::Rejected));
    assert_eq!(restored.email_id, "mail-1");
    assert_eq!(restored.origin, None);
}

#[tokio::test]
async fn leaves_no_row_when_parcel_refuses_a_new_number() {
    let f = fixture().await;
    parcel_answers(
        &f.server,
        400,
        json!({"success": false, "error_message": UNKNOWN_FORMAT}),
    )
    .await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["errorKind"], "rejected");
    assert!(row(&f.store.store, "ABC123").await.is_none());
    // The refused request still counts against Parcel's limit.
    assert_eq!(ledger(&f.store.store).await.unwrap().attempts.len(), 1);
}

#[tokio::test]
async fn maps_parcels_already_added_answer_to_duplicate() {
    let f = fixture().await;
    parcel_answers(
        &f.server,
        400,
        json!({"success": false, "error_message": ALREADY_ADDED}),
    )
    .await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC12345", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "duplicate");
    assert_eq!(out["parcelMessage"], ALREADY_ADDED);
    let stored = row(&f.store.store, "ABC12345").await.unwrap();
    assert_eq!(stored.status, Some(SubmissionStatus::Submitted));
}

#[tokio::test]
async fn treats_a_2xx_unsuccessful_body_as_a_refusal() {
    let f = fixture().await;
    parcel_answers(
        &f.server,
        200,
        json!({"success": false, "error_message": UNKNOWN_FORMAT}),
    )
    .await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["errorKind"], "rejected");
    assert_eq!(out["parcelMessage"], UNKNOWN_FORMAT);
    assert!(row(&f.store.store, "ABC123").await.is_none());
}

#[tokio::test]
async fn stops_at_the_ceiling_shared_with_email_until_the_window_moves() {
    let f = fixture().await;
    parcel_answers(&f.server, 200, json!({"success": true})).await;
    for i in 0..TOOL_CEILING as i64 {
        add_ledger::note(&f.store.store, NOW - WINDOW_MS + 60_000 + i)
            .await
            .unwrap();
    }
    let blocked = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(blocked["status"], "error");
    assert_eq!(blocked["errorKind"], "rate_limited");
    assert_eq!(blocked["retryAfter"], "2027-01-15T08:01:00.000Z");
    assert!(add_requests(&f.server).await.is_empty());
    assert!(row(&f.store.store, "ABC123").await.is_none());

    f.clock.set(NOW + 60_000);
    let later = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(later["status"], "accepted");
}

#[tokio::test]
async fn pauses_after_parcel_answers_429() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path(ADD_PATH))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7200")
                .set_body_json(json!({"success": false, "error_message": "Too many requests"})),
        )
        .mount(&f.server)
        .await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["errorKind"], "rate_limited");
    assert_eq!(out["parcelMessage"], "Too many requests");
    assert!(near(iso_ms(&out["retryAfter"]), NOW + 2 * 60 * 60 * 1000));
    assert!(row(&f.store.store, "ABC123").await.is_none());

    let again = call(
        &f.tool,
        json!({"trackingNumber": "XYZ123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(again["errorKind"], "rate_limited");
    assert_eq!(add_requests(&f.server).await.len(), 1);
}

#[tokio::test]
async fn never_resends_an_uncertain_add() {
    let f = fixture().await;
    parcel_answers(&f.server, 503, json!({})).await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["errorKind"], "uncertain");
    assert_eq!(
        row(&f.store.store, "ABC123").await.unwrap().status,
        Some(SubmissionStatus::Pending)
    );
    let again = call(
        &f.tool,
        json!({"trackingNumber": "abc123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(again["errorKind"], "in_progress");
    assert_eq!(again["duplicateOf"], "ABC123");
    assert_eq!(add_requests(&f.server).await.len(), 1);
}

#[tokio::test]
async fn rejects_invalid_input_before_reserving_or_sending() {
    let f = fixture().await;
    let cases = [
        (
            json!({"trackingNumber": "   ", "carrier": "ups"}),
            "trackingNumber is empty after removing spaces",
        ),
        (
            json!({"trackingNumber": "1Z/99", "carrier": "ups"}),
            "trackingNumber may contain only letters, digits and hyphens",
        ),
        (
            json!({"trackingNumber": "ABC123", "carrier": "canadapost"}),
            "Parcel does not support the carrier code \"canadapost\". Did you mean: cp?",
        ),
        (
            json!({"trackingNumber": "ABC123", "carrier": "zzzz"}),
            "Parcel does not support the carrier code \"zzzz\". Use a code from",
        ),
    ];
    for (input, expected) in cases {
        let error = call(&f.tool, input).await.unwrap_err();
        assert_eq!(error.phase, ToolPhase::Input);
        assert!(error.message.starts_with(expected), "{}", error.message);
    }
    for input in [
        json!({"trackingNumber": "ABC123"}),
        json!({"trackingNumber": "", "carrier": "ups"}),
        json!({"trackingNumber": "ABC123", "carrier": "canada post"}),
        json!({"trackingNumber": "ABC123", "carrier": "ups", "force": true}),
        json!({"trackingNumber": "ABC123", "carrier": "ups", "sendPushConfirmation": "yes"}),
    ] {
        let error = call(&f.tool, input).await.unwrap_err();
        assert_eq!(error.phase, ToolPhase::Input);
    }
    assert!(add_requests(&f.server).await.is_empty());
    assert!(ledger(&f.store.store).await.is_none());
}

#[tokio::test]
async fn answers_not_configured_without_an_api_key() {
    let f = fixture_with(SideEffectMode::Live, false).await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "error");
    assert_eq!(out["errorKind"], "not_configured");
    assert!(f.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn record_mode_never_sends_or_spends_the_limit() {
    let f = fixture_with(SideEffectMode::Record, true).await;
    let out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "accepted");
    assert!(add_requests(&f.server).await.is_empty());
    assert!(ledger(&f.store.store).await.is_none());
}

#[tokio::test]
async fn the_email_pipeline_still_submits_through_the_shared_client() {
    let h = common::harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        common::FakeSubmitter::new(&[]),
        common::ups_only(),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(ADD_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
        .mount(&h.server)
        .await;
    let http = omni_testkit::mock_http(&h.server, &["https://api.parcel.app"]);
    let api = Arc::new(
        ParcelApi::new(
            http.clone(),
            "parcel-key".to_owned(),
            SideEffectMode::Live,
            h.clock.clone(),
        )
        .unwrap()
        .with_ledger(h.store.store.clone()),
    );
    let carriers = Arc::new(
        CarrierDirectory::new(
            PublicHttpClient::new(&http).allow_loopback_for_tests(),
            h.clock.clone(),
        )
        .unwrap(),
    );
    let pipeline = DeliveryPipeline::new(PipelineDeps {
        store: h.store.store.clone(),
        run_logs: RunLogs::new(EventBus::new(64), h.clock.clone()),
        triage: EmailTriage::new(Arc::new(common::YesTriage)),
        carriers: carriers.clone(),
        extractor: always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        submitter: api.clone(),
        self_address: None,
        logs_path: None,
        tz: jiff::tz::TimeZone::UTC,
        tracker: TaskTracker::new(),
        systemic: SystemicReporter::new(h.store.store.clone(), h.pushover.clone()),
    });
    pipeline.handle_emails(&[shipment("mail-9")]).await.unwrap();

    let stored = row(&h.store.store, "1Z999AA10123456784").await.unwrap();
    assert_eq!(stored.status, Some(SubmissionStatus::Submitted));
    assert_eq!(stored.email_id, "mail-9");
    assert_eq!(
        add_requests(&h.server).await[0]["send_push_confirmation"],
        true
    );
    assert_eq!(ledger(&h.store.store).await.unwrap().attempts.len(), 1);

    let tool = omni_parcel::mcp::tools(Arc::new(ParcelAdder::new(
        h.store.store.clone(),
        Some(api),
        Some(carriers),
        TaskTracker::new(),
    )))
    .unwrap()
    .remove(0);
    let out = call(
        &tool,
        json!({"trackingNumber": "1Z999AA10123456784", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["status"], "duplicate");
    assert_eq!(add_requests(&h.server).await.len(), 1);
}

#[tokio::test]
async fn names_a_rejected_api_key_and_treats_a_request_timeout_as_uncertain() {
    let f = fixture().await;
    Mock::given(method("POST"))
        .and(path(ADD_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"success": false, "error_message": "Invalid API key"})),
        )
        .up_to_n_times(1)
        .mount(&f.server)
        .await;
    parcel_answers(&f.server, 408, json!({})).await;

    let refused = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(refused["errorKind"], "rejected");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("PARCEL_API_KEY")
    );
    assert!(row(&f.store.store, "ABC123").await.is_none());

    let timed_out = call(
        &f.tool,
        json!({"trackingNumber": "ABC123", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(timed_out["errorKind"], "uncertain");
    assert_eq!(
        row(&f.store.store, "ABC123").await.unwrap().status,
        Some(SubmissionStatus::Pending)
    );
}

#[tokio::test]
async fn the_email_pipeline_never_resends_an_uncertain_tool_add() {
    let submitter = common::FakeSubmitter::new(&[]);
    let h = common::harness(
        always_extract(deliveries(&[("1Z999AA10123456784", &["ups"])])),
        submitter.clone(),
        common::ups_only(),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(ADD_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let http = omni_testkit::mock_http(&h.server, &["https://api.parcel.app"]);
    let api = Arc::new(
        ParcelApi::new(
            http.clone(),
            "parcel-key".to_owned(),
            SideEffectMode::Live,
            h.clock.clone(),
        )
        .unwrap(),
    );
    let carriers = Arc::new(
        CarrierDirectory::new(
            PublicHttpClient::new(&http).allow_loopback_for_tests(),
            h.clock.clone(),
        )
        .unwrap(),
    );
    let tool = omni_parcel::mcp::tools(Arc::new(ParcelAdder::new(
        h.store.store.clone(),
        Some(api),
        Some(carriers),
        TaskTracker::new(),
    )))
    .unwrap()
    .remove(0);
    let out = call(
        &tool,
        json!({"trackingNumber": "1Z999AA10123456784", "carrier": "ups"}),
    )
    .await
    .unwrap();
    assert_eq!(out["errorKind"], "uncertain");

    h.pipeline
        .handle_emails(&[shipment("mail-3")])
        .await
        .unwrap();
    let stored = row(&h.store.store, "1Z999AA10123456784").await.unwrap();
    assert_eq!(stored.status, Some(SubmissionStatus::Pending));
    assert_eq!(stored.origin, Some(SubmissionOrigin::ParcelsAdd));
    assert!(submitter.calls().is_empty());
    assert_eq!(add_requests(&h.server).await.len(), 1);
}
