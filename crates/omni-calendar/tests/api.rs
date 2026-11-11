//! CalDAV writes, plus the missing-event lookup (deleted or moved to another
//! calendar) and record mode.
//!
//! The interruption cases assert that aborting the task drops the in-flight
//! request. Update and delete requests are bounded by the shared 15 s request
//! timeout, asserted in `http.rs`; these cases check the outcomes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::{CALENDAR_URL, icloud_http, session};
use omni_calendar::caldav::api::MERGE_ATTEMPTS;
use omni_calendar::caldav::http::CALDAV_ERROR_MAX_BYTES;
use omni_calendar::caldav::ics::build_icalendar;
use omni_calendar::caldav::{CaldavWriter, CreateOutcome, DeleteOutcome, UpdateOutcome};
use omni_calendar::extraction::schema::{EventAction, ExtractedEvent};
use omni_http::SideEffectMode;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const UID: &str = "stable@omni-notify";
const EVENT_PATH: &str = "/123/calendars/home/stable@omni-notify.ics";

fn event() -> ExtractedEvent {
    let mut event = ExtractedEvent::new(EventAction::Create, "Dentist", "2026-09-01", false);
    event.start_time = Some("09:00".to_owned());
    event
}

fn writer(server: &MockServer) -> CaldavWriter {
    CaldavWriter::new(
        icloud_http(server),
        omni_testkit::test_clock(omni_testkit::TEST_EPOCH_MS),
        SideEffectMode::Live,
        "America/Vancouver",
    )
}

async fn wait_for_requests(server: &MockServer, n: usize) {
    for _ in 0..200 {
        if server.received_requests().await.unwrap_or_default().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server never received {n} request(s)");
}

#[tokio::test]
async fn aborts_an_in_flight_put_when_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_secs(60)))
        .mount(&server)
        .await;
    let w = writer(&server);
    let task = tokio::spawn(async move { w.create(&session(), &event(), UID).await });
    wait_for_requests(&server, 1).await;
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].method.as_str(), "PUT");
}

#[tokio::test]
async fn aborts_an_in_flight_delete_when_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(60)))
        .mount(&server)
        .await;
    let w = writer(&server);
    let task = tokio::spawn(async move { w.delete(&session(), UID).await });
    wait_for_requests(&server, 1).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].method.as_str(), "DELETE");
}

#[tokio::test]
async fn reconciles_a_repeated_deterministic_create_after_an_ambiguous_response() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(EVENT_PATH))
        .and(header("If-None-Match", "*"))
        .and(header("Authorization", "Basic secret"))
        .respond_with(ResponseTemplate::new(412))
        .expect(1)
        .mount(&server)
        .await;
    let result = writer(&server)
        .create(&session(), &event(), UID)
        .await
        .unwrap();
    assert_eq!(
        result,
        CreateOutcome::AlreadyExists {
            event_uid: UID.to_owned()
        }
    );
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(
        request.headers.get("content-type").unwrap(),
        "text/calendar; charset=utf-8"
    );
    assert_eq!(
        format!("{CALENDAR_URL}{UID}.ics"),
        "https://p42-caldav.icloud.com/123/calendars/home/stable@omni-notify.ics"
    );
}

fn moved() -> ExtractedEvent {
    let mut event = event();
    event.start_time = Some("10:00".to_owned());
    event
}

/// The server copy: what the pipeline wrote plus a manual edit.
fn server_copy(location: &str) -> String {
    build_icalendar(
        &event(),
        UID,
        omni_testkit::TEST_EPOCH_MS,
        "America/Vancouver",
    )
    .replace("END:VEVENT", &format!("LOCATION:{location}\r\nEND:VEVENT"))
}

fn ics(body: String, etag: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("ETag", etag)
        .set_body_string(body)
}

#[tokio::test]
async fn bounds_update_and_delete_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(EVENT_PATH))
        .respond_with(ics(server_copy("Front desk"), "\"e1\""))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(EVENT_PATH))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(EVENT_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let w = writer(&server);
    let update = w.update(&session(), &event(), &moved(), UID).await.unwrap();
    let deletion = w.delete(&session(), UID).await.unwrap();
    assert_eq!(
        update,
        UpdateOutcome::Success {
            event_uid: UID.to_owned()
        }
    );
    assert_eq!(deletion, DeleteOutcome::NotFound);
    // An update is conditional on the version it merged into.
    let requests = server.received_requests().await.unwrap();
    let put = &requests[1];
    assert_eq!(put.method.as_str(), "PUT");
    assert!(put.headers.get("if-none-match").is_none());
    assert_eq!(put.headers.get("if-match").unwrap(), "\"e1\"");
    let body = String::from_utf8_lossy(&put.body);
    assert!(body.contains("T100000\r\n"), "{body}");
    assert!(
        body.contains("LOCATION:Front desk\r\n"),
        "manual edit kept: {body}"
    );
}

#[tokio::test]
async fn update_skips_the_write_when_nothing_the_email_owns_changed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ics(server_copy("Front desk"), "\"e1\""))
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &event(), UID)
        .await
        .unwrap();
    assert!(matches!(result, UpdateOutcome::Success { .. }));
    let requests = server.received_requests().await.unwrap();
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
}

#[tokio::test]
async fn update_merges_again_after_a_concurrent_edit() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ics(server_copy("Front desk"), "\"e1\""))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ics(server_copy("Room 4"), "\"e2\""))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header("If-Match", "\"e1\""))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header("If-Match", "\"e2\""))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert!(matches!(result, UpdateOutcome::Success { .. }));
    let requests = server.received_requests().await.unwrap();
    let last = requests.last().unwrap();
    let body = String::from_utf8_lossy(&last.body);
    assert!(body.contains("LOCATION:Room 4\r\n"), "{body}");
    assert!(body.contains("T100000\r\n"), "{body}");
}

#[tokio::test]
async fn update_gives_up_after_bounded_conflicts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ics(server_copy("Front desk"), "\"e1\""))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert!(
        matches!(result, UpdateOutcome::Error { code: 412, .. }),
        "{result:?}"
    );
    let puts = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .count();
    assert_eq!(puts, MERGE_ATTEMPTS);
}

const MOVED_PATH: &str = "/123/calendars/work/renamed-by-calendar.ics";

/// The account's calendars: the session's own, another event calendar and a
/// task list.
const COLLECTIONS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <response><href>/123/calendars/</href><propstat><prop><resourcetype><collection/></resourcetype></prop></propstat></response>
  <response><href>/123/calendars/home/</href><propstat><prop><displayname>Home</displayname><resourcetype><collection/><c:calendar/></resourcetype><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></prop></propstat></response>
  <response><href>/123/calendars/work/</href><propstat><prop><displayname>Work</displayname><resourcetype><collection/><c:calendar/></resourcetype><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></prop></propstat></response>
  <response><href>/123/calendars/tasks/</href><propstat><prop><displayname>Tasks</displayname><resourcetype><collection/><c:calendar/></resourcetype><c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set></prop></propstat></response>
</multistatus>"#;

/// A `calendar-query` answer: `(href, uid)` of each event near the date.
fn multistatus(events: &[(&str, &str)]) -> ResponseTemplate {
    let responses: String = events
        .iter()
        .map(|(href, uid)| {
            format!("<response><href>{href}</href><propstat><prop><getetag>\"x\"</getetag><calendar-data xmlns=\"urn:ietf:params:xml:ns:caldav\">BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:{uid}\nEND:VEVENT\nEND:VCALENDAR\n</calendar-data></prop></propstat></response>")
        })
        .collect();
    ResponseTemplate::new(207).set_body_string(format!(
        r#"<?xml version="1.0" encoding="utf-8"?><multistatus xmlns="DAV:">{responses}</multistatus>"#
    ))
}

/// The event is gone from its calendar; listing the other event calendar
/// around the event's date answers with `matches`.
async fn mount_missing_at_home(server: &MockServer, matches: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(EVENT_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/123/calendars/"))
        .and(header("Depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(COLLECTIONS))
        .mount(server)
        .await;
    Mock::given(method("REPORT"))
        .and(path("/123/calendars/work/"))
        .and(header("Authorization", "Basic secret"))
        .respond_with(matches)
        .mount(server)
        .await;
}

async fn calls(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url.path()))
        .collect()
}

#[tokio::test]
async fn update_merges_into_an_event_moved_to_another_calendar() {
    let server = MockServer::start().await;
    mount_missing_at_home(
        &server,
        multistatus(&[
            ("/123/calendars/work/neighbour.ics", "someone-else"),
            (MOVED_PATH, UID),
        ]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(MOVED_PATH))
        .and(header("Authorization", "Basic secret"))
        .respond_with(ics(server_copy("Front desk"), "\"w1\""))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path(MOVED_PATH))
        .and(header("If-Match", "\"w1\""))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert_eq!(
        result,
        UpdateOutcome::Success {
            event_uid: UID.to_owned()
        }
    );
    // Found by reading: only the other event calendar is listed, around the
    // event's date, and nothing is ever PUT at the old location.
    assert_eq!(
        calls(&server).await,
        [
            "GET /123/calendars/home/stable@omni-notify.ics",
            "PROPFIND /123/calendars/",
            "REPORT /123/calendars/work/",
            "GET /123/calendars/work/renamed-by-calendar.ics",
            "GET /123/calendars/work/renamed-by-calendar.ics",
            "PUT /123/calendars/work/renamed-by-calendar.ics",
        ]
    );
    let requests = server.received_requests().await.unwrap();
    let query = String::from_utf8_lossy(&requests[2].body);
    assert!(
        query.contains(r#"<c:time-range start="20260831T000000Z" end="20260903T000000Z"/>"#),
        "{query}"
    );
    assert!(
        !query.contains("prop-filter"),
        "iCloud answers 412: {query}"
    );
}

#[tokio::test]
async fn update_reports_a_deleted_event_missing_and_writes_nothing() {
    let server = MockServer::start().await;
    mount_missing_at_home(&server, multistatus(&[])).await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert_eq!(result, UpdateOutcome::Missing);
    assert_eq!(
        calls(&server).await,
        [
            "GET /123/calendars/home/stable@omni-notify.ics",
            "PROPFIND /123/calendars/",
            "REPORT /123/calendars/work/",
        ]
    );
}

#[tokio::test]
async fn update_ignores_lookup_matches_that_are_untrusted_or_another_event() {
    let server = MockServer::start().await;
    mount_missing_at_home(
        &server,
        multistatus(&[
            ("https://attacker.example/x.ics", UID),
            ("/123/calendars/elsewhere/x.ics", UID),
            // Listed with a UID this one merely starts.
            ("/123/calendars/work/other.ics", "stable@omni-notify-2"),
        ]),
    )
    .await;
    // Its own body carries another UID.
    Mock::given(method("GET"))
        .and(path("/123/calendars/work/other.ics"))
        .respond_with(ics(
            server_copy("Front desk").replace("UID:stable@omni-notify", "UID:stable@omni-notify-2"),
            "\"o1\"",
        ))
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert_eq!(result, UpdateOutcome::Missing);
    assert_eq!(
        calls(&server).await,
        [
            "GET /123/calendars/home/stable@omni-notify.ics",
            "PROPFIND /123/calendars/",
            "REPORT /123/calendars/work/",
            "GET /123/calendars/work/other.ics",
        ]
    );
}

#[tokio::test]
async fn update_skips_a_calendar_that_refuses_the_lookup() {
    let server = MockServer::start().await;
    mount_missing_at_home(&server, ResponseTemplate::new(403)).await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert_eq!(result, UpdateOutcome::Missing);
}

#[tokio::test]
async fn update_does_not_call_an_event_deleted_when_its_calendar_is_gone() {
    let server = MockServer::start().await;
    // Every event 404s, and the account no longer lists the write calendar.
    Mock::given(method("PROPFIND"))
        .and(path("/123/calendars/"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            COLLECTIONS.replace("/123/calendars/home/", "/123/calendars/renamed/"),
        ))
        .mount(&server)
        .await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert!(
        matches!(&result, UpdateOutcome::Error { code: 404, message } if message.contains("not in the account's calendar list")),
        "{result:?}"
    );
    assert_eq!(
        calls(&server).await,
        [
            "GET /123/calendars/home/stable@omni-notify.ics",
            "PROPFIND /123/calendars/",
        ]
    );
}

#[tokio::test]
async fn update_that_cannot_finish_the_lookup_is_an_error_and_writes_nothing() {
    let server = MockServer::start().await;
    mount_missing_at_home(&server, ResponseTemplate::new(503)).await;
    let result = writer(&server)
        .update(&session(), &event(), &moved(), UID)
        .await
        .unwrap();
    assert_eq!(
        result,
        UpdateOutcome::Error {
            code: 503,
            message: "could not look for the moved event (CalDAV 503: Service Unavailable)"
                .to_owned()
        }
    );
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !matches!(r.method.as_str(), "PUT" | "DELETE"))
    );
}

#[tokio::test]
async fn rejects_an_oversized_write_error_response_instead_of_buffering_it() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(500).set_body_bytes(vec![b'x'; CALDAV_ERROR_MAX_BYTES + 1]),
        )
        .mount(&server)
        .await;
    let error = writer(&server)
        .create(&session(), &event(), UID)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "response exceeds the {CALDAV_ERROR_MAX_BYTES}-byte limit"
        )),
        "{error}"
    );
    assert!(!error.transient);
}

#[tokio::test]
async fn reports_http_failures_with_their_status() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&server)
        .await;
    let result = writer(&server)
        .create(&session(), &event(), UID)
        .await
        .unwrap();
    assert_eq!(
        result,
        CreateOutcome::Error {
            code: 503,
            message: "CalDAV 503: Service Unavailable".to_owned()
        }
    );
}

#[tokio::test]
async fn record_mode_captures_writes_without_sending() {
    let server = MockServer::start().await;
    let w = CaldavWriter::new(
        icloud_http(&server),
        omni_testkit::test_clock(omni_testkit::TEST_EPOCH_MS),
        SideEffectMode::Record,
        "America/Vancouver",
    );
    assert!(matches!(
        w.create(&session(), &event(), UID).await.unwrap(),
        CreateOutcome::Success { .. }
    ));
    assert_eq!(
        w.delete(&session(), UID).await.unwrap(),
        DeleteOutcome::Success
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let recorded = w.recorded();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].method, "PUT");
    assert_eq!(recorded[0].url, format!("{CALENDAR_URL}{UID}.ics"));
    assert!(
        recorded[0]
            .body
            .as_deref()
            .unwrap()
            .contains("UID:stable@omni-notify\r\n")
    );
    assert_eq!(recorded[1].method, "DELETE");
}
