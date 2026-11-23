//! Shared platform fetching. A dropped reqwest response closes the
//! connection, so the cases assert the typed error only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_http::public::PublicHttpClient;
use omni_live::platforms::{fetch_gql, fetch_page_html};
use serde::Deserialize;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Debug, Deserialize, PartialEq)]
struct Ok {
    ok: bool,
}

#[tokio::test]
async fn rejects_a_fixed_length_oversized_page_before_buffering_it() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/live"))
        .respond_with(ResponseTemplate::new(200).set_body_string("123456789"))
        .mount(&server)
        .await;
    let http = common::public_client(&server, &["https://example.com"]);
    let error = fetch_page_html(&http, "https://example.com/live", 8)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("Response exceeds the 8-byte limit"),
        "{error}"
    );
}

#[tokio::test]
async fn cancels_a_chunked_oversized_gql_response() {
    let base = common::chunked_server(vec!["12345", "67890"]).await;
    let http = PublicHttpClient::new(&omni_testkit::no_network()).allow_loopback_for_tests();
    let error = fetch_gql::<Ok>(&http, &format!("{base}/gql"), "client", "{}", 8)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("Response exceeds the 8-byte limit"),
        "{error}"
    );
}

#[tokio::test]
async fn decodes_a_valid_gql_response_with_the_concrete_schema() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&server)
        .await;
    let http = common::public_client(&server, &["https://example.com"]);
    let decoded: Ok = fetch_gql(&http, "https://example.com/gql", "client", "{}", 1024)
        .await
        .unwrap();
    assert_eq!(decoded, Ok { ok: true });
}
