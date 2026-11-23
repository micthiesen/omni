//! Twitch live status, including the escaped-login query.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_live::platform::{FetchedLive, FetchedStatus};
use omni_live::platforms::{TwitchClient, TwitchGqlResponse, extract_twitch_status, twitch_query};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

fn decode(value: serde_json::Value) -> TwitchGqlResponse {
    serde_json::from_value(value).unwrap()
}

fn live(title: &str, viewers: i64, category: Option<&str>) -> FetchedStatus {
    FetchedStatus::Live(FetchedLive {
        title: title.into(),
        viewer_count: Some(viewers),
        category: category.map(str::to_owned),
        started_at: None,
    })
}

#[test]
fn should_prefer_live_up_notification_over_stream_title() {
    let data = decode(json!({"data": {"user": {
        "stream": {"title": "Playing games", "viewersCount": 15000, "game": {"name": "Elden Ring"}},
        "broadcastSettings": {"liveUpNotification": "Custom notification message"}
    }}}));
    assert_eq!(
        extract_twitch_status(&data),
        live("Custom notification message", 15000, Some("Elden Ring"))
    );
}

#[test]
fn should_fall_back_to_stream_title_when_live_up_notification_is_null() {
    let data = decode(json!({"data": {"user": {
        "stream": {"title": "Playing games", "viewersCount": 15000, "game": {"name": "Just Chatting"}},
        "broadcastSettings": {"liveUpNotification": null}
    }}}));
    assert_eq!(
        extract_twitch_status(&data),
        live("Playing games", 15000, Some("Just Chatting"))
    );
}

#[test]
fn should_fall_back_to_stream_title_when_live_up_notification_is_empty() {
    let data = decode(json!({"data": {"user": {
        "stream": {"title": "Playing games", "viewersCount": 15000, "game": null},
        "broadcastSettings": {"liveUpNotification": ""}
    }}}));
    assert_eq!(
        extract_twitch_status(&data),
        live("Playing games", 15000, None)
    );
}

#[test]
fn should_return_offline_when_stream_is_null() {
    let data = decode(
        json!({"data": {"user": {"stream": null, "broadcastSettings": {"liveUpNotification": null}}}}),
    );
    assert_eq!(extract_twitch_status(&data), FetchedStatus::Offline);
}

#[test]
fn should_return_offline_when_user_is_null() {
    assert_eq!(
        extract_twitch_status(&decode(json!({"data": {"user": null}}))),
        FetchedStatus::Offline
    );
}

#[test]
fn escapes_the_login_inside_the_graphql_document() {
    assert_eq!(
        twitch_query("jerma985"),
        r#"query{user(login:"jerma985"){stream{title viewersCount game{name}}broadcastSettings{liveUpNotification}}}"#
    );
    let hostile = twitch_query(r#"x"){a}#\"#);
    assert!(hostile.contains(r#"login:"x\"){a}#\\""#), "{hostile}");
}

#[tokio::test]
async fn fetches_through_gql_and_reports_failures_as_unknown() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("POST"))
        .and(path("/gql"))
        .and(header("Client-Id", "kimne78kx3ncx6brgo4mv6wki5h1ko"))
        .and(body_json(json!({"query": twitch_query("jerma985")})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"user": {
                "stream": {"title": "Live", "viewersCount": 12, "game": null},
                "broadcastSettings": {"liveUpNotification": null}
            }}})),
        )
        .mount(&server)
        .await;
    let client = TwitchClient::new(common::public_client(&server, &["https://gql.twitch.tv"]));
    assert_eq!(
        client.fetch_live_status("jerma985").await,
        live("Live", 12, None)
    );
    let FetchedStatus::Unknown { error } = client.fetch_live_status("missing").await else {
        panic!("unknown expected");
    };
    assert!(
        error.starts_with("Failed to fetch GQL from https://gql.twitch.tv/gql"),
        "{error}"
    );
}
