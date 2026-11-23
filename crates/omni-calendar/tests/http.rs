//! CalDAV HTTP safety: timeouts, cancellation and response size limits.
//!
//! "cancels a chunked response as soon as it exceeds the XML limit" runs
//! against a raw local server streaming endless 1 MiB chunks; it asserts the
//! read stops with the limit error and the server sees the connection close
//! after a bounded number of chunks (socket buffers make the exact count
//! platform dependent, so the bound is loose).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::icloud_http;
use omni_calendar::caldav::http::{
    CALDAV_REQUEST_TIMEOUT, CALDAV_XML_MAX_BYTES, assert_trusted_caldav_url, propfind,
};
use omni_http::{HttpOverrides, Url};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn root() -> Url {
    Url::parse("https://caldav.icloud.com/").unwrap()
}

#[tokio::test]
async fn aborts_an_in_flight_propfind_when_dropped() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_delay(Duration::from_secs(60)))
        .mount(&server)
        .await;
    let http = icloud_http(&server);
    let task =
        tokio::spawn(
            async move { propfind(&http, &root(), "Basic secret", "0", "<propfind/>").await },
        );
    for _ in 0..200 {
        if !server.received_requests().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn follows_icloud_shard_redirects_with_authorization_and_a_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://p42-caldav.icloud.com/123/"),
        )
        .mount(&server)
        .await;
    Mock::given(method("PROPFIND"))
        .and(path("/123/"))
        .respond_with(ResponseTemplate::new(207).set_body_string("<multistatus/>"))
        .mount(&server)
        .await;
    let result = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap();
    assert_eq!(result.url.as_str(), "https://p42-caldav.icloud.com/123/");
    assert_eq!(result.xml, "<multistatus/>");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Basic secret"
        );
        assert_eq!(request.headers.get("depth").unwrap(), "0");
    }
    assert_eq!(CALDAV_REQUEST_TIMEOUT, Duration::from_millis(15_000));
}

#[tokio::test]
async fn refuses_to_forward_authorization_to_an_untrusted_redirect_host() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "https://attacker.example/collect"),
        )
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Refusing to forward CalDAV credentials"),
        "{error}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn refuses_a_downgrade_to_http_on_the_same_host() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", "http://caldav.icloud.com/"),
        )
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Refusing to forward"), "{error}");
}

#[tokio::test]
async fn stops_after_five_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/again/"))
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("exceeded 5 redirects"),
        "{error}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 6);
}

#[test]
fn accepts_only_icloud_https_collection_urls() {
    assert!(
        assert_trusted_caldav_url("https://p03-caldav.icloud.com/123/calendars/home/")
            .unwrap()
            .as_str()
            .contains("p03-caldav.icloud.com")
    );
    let insecure = assert_trusted_caldav_url("http://caldav.icloud.com/").unwrap_err();
    assert!(insecure.to_string().contains("Untrusted iCloud CalDAV URL"));
    let lookalike = assert_trusted_caldav_url("https://icloud.com.attacker.example/").unwrap_err();
    assert!(
        lookalike
            .to_string()
            .contains("Untrusted iCloud CalDAV URL")
    );
}

#[tokio::test]
async fn rejects_a_response_whose_content_length_exceeds_the_xml_limit() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .and(header("depth", "0"))
        .respond_with(
            ResponseTemplate::new(207).set_body_bytes(vec![b'<'; CALDAV_XML_MAX_BYTES + 1]),
        )
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "response exceeds the {CALDAV_XML_MAX_BYTES}-byte limit"
        )),
        "{error}"
    );
}

#[tokio::test]
async fn cancels_a_chunked_response_as_soon_as_it_exceeds_the_xml_limit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let chunks = Arc::new(AtomicUsize::new(0));
    let sent = chunks.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0u8; 8192];
        let _ = socket.read(&mut request).await;
        socket
            .write_all(b"HTTP/1.1 207 Multi-Status\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        let chunk = vec![b'<'; 1024 * 1024];
        let header = format!("{:x}\r\n", chunk.len());
        loop {
            let wrote = async {
                socket.write_all(header.as_bytes()).await?;
                socket.write_all(&chunk).await?;
                socket.write_all(b"\r\n").await
            }
            .await;
            if wrote.is_err() {
                break;
            }
            if sent.fetch_add(1, Ordering::SeqCst) > 512 {
                break;
            }
        }
    });
    let http = omni_testkit::no_network().with_overrides(HttpOverrides {
        rewrites: vec![(root(), Url::parse(&format!("http://{address}")).unwrap())],
    });
    let error = propfind(&http, &root(), "Basic secret", "0", "<propfind/>")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "response exceeds the {CALDAV_XML_MAX_BYTES}-byte limit"
        )),
        "{error}"
    );
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("server stops once the client hangs up")
        .unwrap();
    assert!(
        chunks.load(Ordering::SeqCst) < 64,
        "{} chunks",
        chunks.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn reports_error_statuses_with_their_body() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(401).set_body_string("denied"))
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert_eq!(error.status, Some(401));
    assert!(!error.transient);
    assert_eq!(
        error.to_string(),
        "CalDAV PROPFIND failed: 401 Unauthorized (https://caldav.icloud.com/)\ndenied"
    );
}

#[tokio::test]
async fn rejects_a_non_xml_success_body() {
    let server = MockServer::start().await;
    Mock::given(method("PROPFIND"))
        .respond_with(ResponseTemplate::new(207).set_body_string("{\"not\":\"xml\"}"))
        .mount(&server)
        .await;
    let error = propfind(
        &icloud_http(&server),
        &root(),
        "Basic secret",
        "0",
        "<propfind/>",
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "decode CalDAV XML failed: CalDAV response is not XML"
    );
}
