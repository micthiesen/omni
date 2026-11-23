//! RFC 6764 iCloud discovery: principal → calendar home → VEVENT collection, following the
//! shard redirect, never assuming a `pXX` host, resolving relative hrefs
//! against the URL that answered.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{icloud_http, settings};
use omni_calendar::caldav::discover_icloud_calendar;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PRINCIPAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:"><response><href>/</href><propstat><prop>
<current-user-principal><href>/987/principal/</href></current-user-principal>
</prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;

const HOME: &str = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:response><d:href>/987/principal/</d:href><d:propstat><d:prop>
<c:calendar-home-set><d:href>/987/calendars/</d:href></c:calendar-home-set>
</d:prop></d:propstat></d:response></d:multistatus>"#;

const COLLECTIONS: &str = r#"<multistatus xmlns="DAV:">
<response><href>/987/calendars/tasks/</href><propstat><prop>
<displayname>Reminders</displayname>
<resourcetype><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
<supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name='VTODO'/></supported-calendar-component-set>
</prop></propstat></response>
<response><href>/987/calendars/ABCD/</href><propstat><prop>
<displayname>Personal</displayname>
<resourcetype><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
<supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name='VEVENT'/></supported-calendar-component-set>
</prop></propstat></response>
</multistatus>"#;

async fn mount_chain(server: &MockServer, collections: &str) {
    // The well-known root answers with a redirect to the account's shard.
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .and(body_string_contains("current-user-principal"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", "https://p07-caldav.icloud.com/"),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .and(header("depth", "0"))
        .respond_with(ResponseTemplate::new(207).set_body_string(PRINCIPAL))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/987/principal/"))
        .and(body_string_contains("calendar-home-set"))
        .respond_with(ResponseTemplate::new(207).set_body_string(HOME))
        .mount(server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/987/calendars/"))
        .and(header("depth", "1"))
        .respond_with(ResponseTemplate::new(207).set_body_string(collections.to_owned()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn follows_the_rfc6764_chain_to_the_shard_and_picks_a_vevent_calendar() {
    let server = MockServer::start().await;
    mount_chain(&server, COLLECTIONS).await;
    let session = discover_icloud_calendar(&icloud_http(&server), &settings(None))
        .await
        .unwrap();
    // Relative hrefs resolved against the shard that answered, not the root.
    assert_eq!(
        session.calendar_url,
        "https://p07-caldav.icloud.com/987/calendars/ABCD/"
    );
    assert_eq!(
        session.auth_header,
        "Basic dXNlckBpY2xvdWQuY29tOmFwcC1wYXNzd29yZA=="
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|r| r.headers.get("authorization").unwrap()
                == "Basic dXNlckBpY2xvdWQuY29tOmFwcC1wYXNzd29yZA==")
    );
}

#[tokio::test]
async fn reports_the_collections_it_saw_when_none_is_usable() {
    let server = MockServer::start().await;
    let todo_only = COLLECTIONS
        .split("<response><href>/987/calendars/ABCD/")
        .next()
        .unwrap()
        .to_owned()
        + "</multistatus>";
    mount_chain(&server, &todo_only).await;
    let error = discover_icloud_calendar(&icloud_http(&server), &settings(None))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "discover iCloud calendar collection failed: no usable calendar collection found (saw: Reminders)"
    );
    assert!(!error.transient);
}

#[tokio::test]
async fn rejects_a_principal_on_an_untrusted_host() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string(
            "<multistatus><response><propstat><prop><current-user-principal><href>https://evil.example/p/</href></current-user-principal></prop></propstat></response></multistatus>",
        ))
        .mount(&server)
        .await;
    let error = discover_icloud_calendar(&icloud_http(&server), &settings(None))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Untrusted iCloud CalDAV URL: https://evil.example")
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_configured_calendar_url_skips_discovery_and_gains_a_trailing_slash() {
    let server = MockServer::start().await;
    let session = discover_icloud_calendar(
        &icloud_http(&server),
        &settings(Some("https://p03-caldav.icloud.com/1/calendars/home")),
    )
    .await
    .unwrap();
    assert_eq!(
        session.calendar_url,
        "https://p03-caldav.icloud.com/1/calendars/home/"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let error = discover_icloud_calendar(
        &icloud_http(&server),
        &settings(Some("https://calendar.example/home/")),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Untrusted iCloud CalDAV URL"));
}

#[tokio::test]
async fn missing_principal_is_a_permanent_error() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string("<multistatus/>"))
        .mount(&server)
        .await;
    let error = discover_icloud_calendar(&icloud_http(&server), &settings(None))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "discover iCloud principal failed: no current-user-principal in PROPFIND response"
    );
}
