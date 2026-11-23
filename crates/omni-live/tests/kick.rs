//! Kick live status, plus the token cache and
//! 401 refresh.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_http::public::PublicHttpClient;
use omni_http::{HttpOverrides, Url};
use omni_live::platform::{FetchedLive, FetchedStatus};
use omni_live::platforms::{
    KickCategory, KickChannel, KickChannelsResponse, KickClient, KickCredentials, KickStream,
    extract_kick_status,
};
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

fn base_channel() -> KickChannel {
    KickChannel {
        slug: "destiny".into(),
        stream_title: "ultra boring work/emails".into(),
        category: Some(KickCategory {
            id: Some(15.0),
            name: Some("Just Chatting".into()),
        }),
        stream: Some(KickStream {
            is_live: true,
            viewer_count: Some(3952.0),
            start_time: None,
        }),
    }
}

fn response(channels: Vec<KickChannel>) -> KickChannelsResponse {
    KickChannelsResponse {
        data: channels,
        message: Some("success".into()),
    }
}

fn live(title: &str, viewers: Option<i64>, category: Option<&str>) -> FetchedStatus {
    FetchedStatus::Live(FetchedLive {
        title: title.into(),
        viewer_count: viewers,
        category: category.map(str::to_owned),
        started_at: None,
    })
}

#[test]
fn returns_live_with_title_viewers_and_category_when_is_live_is_true() {
    assert_eq!(
        extract_kick_status(&response(vec![base_channel()])),
        live(
            "ultra boring work/emails",
            Some(3952),
            Some("Just Chatting")
        )
    );
}

#[test]
fn returns_offline_when_the_stream_object_is_missing() {
    let channel = KickChannel {
        stream: None,
        ..base_channel()
    };
    assert_eq!(
        extract_kick_status(&response(vec![channel])),
        FetchedStatus::Offline
    );
}

#[test]
fn returns_offline_when_is_live_is_false() {
    let channel = KickChannel {
        stream: Some(KickStream {
            is_live: false,
            viewer_count: None,
            start_time: None,
        }),
        ..base_channel()
    };
    assert_eq!(
        extract_kick_status(&response(vec![channel])),
        FetchedStatus::Offline
    );
}

#[test]
fn returns_offline_when_no_channel_matches_the_slug() {
    assert_eq!(
        extract_kick_status(&response(vec![])),
        FetchedStatus::Offline
    );
}

#[test]
fn falls_back_to_slug_when_stream_title_is_empty() {
    let channel = KickChannel {
        stream_title: String::new(),
        ..base_channel()
    };
    assert_eq!(
        extract_kick_status(&response(vec![channel])),
        live("destiny", Some(3952), Some("Just Chatting"))
    );
}

#[test]
fn omits_category_when_not_present() {
    let channel = KickChannel {
        category: None,
        ..base_channel()
    };
    assert_eq!(
        extract_kick_status(&response(vec![channel])),
        live("ultra boring work/emails", Some(3952), None)
    );
}

#[test]
fn omits_viewer_count_when_api_omits_it() {
    let channel = KickChannel {
        stream: Some(KickStream {
            is_live: true,
            viewer_count: None,
            start_time: None,
        }),
        ..base_channel()
    };
    assert_eq!(
        extract_kick_status(&response(vec![channel])),
        live("ultra boring work/emails", None, Some("Just Chatting"))
    );
}

#[test]
fn decodes_missing_stream_title_as_empty() {
    let decoded: KickChannelsResponse =
        serde_json::from_value(json!({"data": [{"slug": "destiny", "stream": {"is_live": true}}]}))
            .unwrap();
    assert_eq!(extract_kick_status(&decoded), live("destiny", None, None));
}

fn credentials() -> Option<KickCredentials> {
    Some(KickCredentials {
        client_id: "client".into(),
        client_secret: "secret".into(),
    })
}

fn client_for(token_base: &str, channels_base: &str) -> PublicHttpClient {
    let rewrites = vec![
        (
            Url::parse("https://id.kick.com").unwrap(),
            Url::parse(token_base).unwrap(),
        ),
        (
            Url::parse("https://api.kick.com").unwrap(),
            Url::parse(channels_base).unwrap(),
        ),
    ];
    PublicHttpClient::new(&omni_testkit::no_network().with_overrides(HttpOverrides { rewrites }))
        .allow_loopback_for_tests()
}

fn token_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(json!({"access_token": "token", "token_type": "Bearer", "expires_in": 3600}))
}

#[tokio::test]
async fn rejects_a_fixed_length_oversized_token_response_before_buffering() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("123456789"))
        .mount(&server)
        .await;
    let client = KickClient::new(client_for(&server.uri(), &server.uri()), credentials())
        .with_limits(8, 1024);
    let FetchedStatus::Unknown { error } = client.fetch_live_status("destiny").await else {
        panic!("unknown expected");
    };
    assert!(
        error.contains("Response exceeds the 8-byte limit"),
        "{error}"
    );
}

#[tokio::test]
async fn cancels_a_chunked_oversized_channels_response() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(token_response())
        .mount(&server)
        .await;
    let chunked = common::chunked_server(vec!["12345", "67890"]).await;
    let client =
        KickClient::new(client_for(&server.uri(), &chunked), credentials()).with_limits(1024, 8);
    let FetchedStatus::Unknown { error } = client.fetch_live_status("destiny").await else {
        panic!("unknown expected");
    };
    assert!(
        error.contains("Response exceeds the 8-byte limit"),
        "{error}"
    );
}

#[tokio::test]
async fn reports_missing_credentials_as_unknown() {
    let server = omni_testkit::mock_server().await;
    let client = KickClient::new(client_for(&server.uri(), &server.uri()), None);
    let FetchedStatus::Unknown { error } = client.fetch_live_status("destiny").await else {
        panic!("unknown expected");
    };
    assert!(
        error.contains("KICK_CLIENT_ID and KICK_CLIENT_SECRET"),
        "{error}"
    );
}

#[tokio::test]
async fn caches_the_token_and_refreshes_it_once_after_a_401() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(token_response())
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/channels"))
        .and(query_param("slug", "destiny"))
        .and(header("Authorization", "Bearer token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{
            "slug": "destiny", "stream_title": "t", "category": null, "stream": {"is_live": true, "viewer_count": 5}
        }]})))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/public/v1/channels"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired"))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    let client = KickClient::new(client_for(&server.uri(), &server.uri()), credentials());
    assert_eq!(
        client.fetch_live_status("destiny").await,
        live("t", Some(5), None)
    );
    assert_eq!(
        client.fetch_live_status("destiny").await,
        live("t", Some(5), None)
    );
    // Third call: the 401 mock remains, the token is refreshed once, then the retry also fails.
    let FetchedStatus::Unknown { error } = client.fetch_live_status("destiny").await else {
        panic!("unknown expected");
    };
    assert_eq!(error, "Kick API returned 401: expired");
}
