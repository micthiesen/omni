//! App notifications: the APNs route tried before Pushover, the Pushover
//! fallback rule and the alert payload.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_alerts::{NotificationSender, PushOutcome, Pushover, PushoverChannel, PushoverMessage};
use omni_api::ios::{ApnsEnvironment, IosDeviceKind};
use omni_http::SideEffectMode;
use omni_ios_controls::alerts::{AppAlertRoute, MAX_ALERT_PAYLOAD_BYTES, alert_payload};
use omni_ios_controls::apns::{ApnsPushKind, ApnsPushResult, ApnsSender, ApnsTransportError};
use omni_ios_controls::devices::{DeviceInput, list_devices, upsert_device};
use serde_json::{Value, json};

const NOW_MS: i64 = 1_790_000_000_000;

fn token(byte: &str) -> String {
    byte.repeat(32)
}

fn device(id: &str, kind: IosDeviceKind, apns_token: Option<String>, alerts: bool) -> DeviceInput {
    DeviceInput {
        device_id: id.into(),
        name: format!("{id} name"),
        kind,
        apns_token,
        environment: ApnsEnvironment::Sandbox,
        alerts_enabled: alerts,
        app_version: Some("1.0 (1)".into()),
    }
}

fn pushover() -> Pushover {
    Pushover::with_credentials(
        omni_testkit::no_network(),
        Some("user".into()),
        [(PushoverChannel::General, "general".into())],
        SideEffectMode::Record,
    )
}

struct Harness {
    store: omni_testkit::TestStore,
    pushover: Pushover,
    apns: Arc<common::FakeApns>,
}

async fn harness(default: common::Reply, devices: Vec<DeviceInput>) -> Harness {
    let (store, clock) = common::store(NOW_MS).await;
    for input in devices {
        upsert_device(&store.store, input).await.unwrap();
    }
    let apns = common::FakeApns::new(default);
    let pushover = pushover();
    pushover
        .set_app_route(Arc::new(AppAlertRoute::new(
            store.store.clone(),
            Some(apns.clone() as Arc<dyn ApnsSender>),
            clock,
        )))
        .unwrap();
    Harness {
        store,
        pushover,
        apns,
    }
}

fn message() -> PushoverMessage {
    PushoverMessage {
        message: "Something happened".into(),
        title: Some("Omni".into()),
        ..PushoverMessage::default()
    }
}

#[tokio::test(start_paused = true)]
async fn without_an_alert_device_pushover_sends_it() {
    let h = harness(
        Ok(ApnsPushResult::Sent),
        vec![
            device("ipad-device", IosDeviceKind::Ipad, Some(token("ab")), false),
            device("tokenless-phone", IosDeviceKind::Iphone, None, true),
        ],
    )
    .await;
    let outcome = h
        .pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Recorded);
    assert_eq!(h.pushover.recorded().len(), 1);
    assert!(h.apns.pushes().is_empty());
}

#[tokio::test(start_paused = true)]
async fn without_apns_configured_pushover_sends_it() {
    let (store, clock) = common::store(0).await;
    upsert_device(
        &store.store,
        device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        ),
    )
    .await
    .unwrap();
    let pushover = pushover();
    pushover
        .set_app_route(Arc::new(AppAlertRoute::new(
            store.store.clone(),
            None,
            clock,
        )))
        .unwrap();
    assert_eq!(
        pushover
            .send(PushoverChannel::General, message())
            .await
            .unwrap(),
        PushOutcome::Recorded
    );
}

#[tokio::test(start_paused = true)]
async fn an_accepted_app_push_replaces_pushover() {
    let h = harness(
        Ok(ApnsPushResult::Sent),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    let outcome = h
        .pushover
        .send(PushoverChannel::Calendar, message())
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Sent);
    assert!(h.pushover.recorded().is_empty());
    let pushes = h.apns.pushes();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0].kind, ApnsPushKind::Alert);
    assert_eq!(pushes[0].device_token, token("ab"));
    let payload: Value = serde_json::from_str(&pushes[0].payload).unwrap();
    assert_eq!(payload["omni"]["channel"], "calendar");
    assert_eq!(payload["aps"]["thread-id"], "calendar");
}

#[tokio::test(start_paused = true)]
async fn one_accepting_device_delivers_even_when_another_is_uncertain() {
    let h = harness(
        Ok(ApnsPushResult::Sent),
        vec![
            device("iphone-one", IosDeviceKind::Iphone, Some(token("ab")), true),
            device("iphone-two", IosDeviceKind::Iphone, Some(token("cd")), true),
        ],
    )
    .await;
    h.apns.then(Err(ApnsTransportError {
        operation: "request".into(),
        detail: "reset".into(),
        not_sent: false,
    }));
    assert_eq!(
        h.pushover
            .send(PushoverChannel::General, message())
            .await
            .unwrap(),
        PushOutcome::Sent
    );
    assert!(h.pushover.recorded().is_empty());
}

#[tokio::test(start_paused = true)]
async fn invalid_tokens_are_cleared_and_pushover_sends_it() {
    let h = harness(
        Ok(ApnsPushResult::InvalidToken {
            reason: "Unregistered".into(),
        }),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    let outcome = h
        .pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Recorded);
    let devices = list_devices(&h.store.store).await.unwrap();
    assert_eq!(devices[0].apns_token, None);
    assert!(devices[0].alerts_enabled);
    // The device is no longer pushable: the next message goes straight to Pushover.
    h.apns.clear();
    h.pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert!(h.apns.pushes().is_empty());
    assert_eq!(h.pushover.recorded().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn server_errors_retry_once_then_fall_back_to_pushover() {
    let h = harness(
        common::failed(503, "ServiceUnavailable"),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    let outcome = h
        .pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Recorded);
    assert_eq!(h.apns.pushes().len(), 2);
    // A definite rejection is not retried.
    h.apns.clear();
    h.apns
        .set_default(common::failed(403, "InvalidProviderToken"));
    h.pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert_eq!(h.apns.pushes().len(), 1);
    assert_eq!(h.pushover.recorded().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_push_without_an_answer_is_uncertain_and_never_falls_back() {
    let h = harness(
        Err(ApnsTransportError {
            operation: "request".into(),
            detail: "APNs alert push timed out".into(),
            not_sent: false,
        }),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    let error = h
        .pushover
        .send_with_token("streamer-app", message())
        .await
        .unwrap_err();
    assert_eq!(error.status, None);
    assert!(error.to_string().contains("timed out"), "{error}");
    assert!(h.pushover.recorded().is_empty());
    assert_eq!(
        h.apns.pushes().len(),
        1,
        "an uncertain push is never retried"
    );
    let payload: Value = serde_json::from_str(&h.apns.pushes()[0].payload).unwrap();
    assert_eq!(payload["omni"]["channel"], "live");
}

#[test]
fn builds_the_alert_payload_with_link_sender_and_widget_refresh() {
    let message = PushoverMessage {
        message: "Debate night".into(),
        title: Some("Destiny is LIVE!".into()),
        url: Some("https://kick.com/destiny".into()),
        url_title: Some("Watch on Kick".into()),
        thread_id: Some("live:destiny".into()),
        sender: Some(NotificationSender {
            id: "destiny".into(),
            name: "Destiny".into(),
            avatar_url: Some("https://files.kick.com/destiny.webp".into()),
        }),
        refresh_widgets: vec!["on-air".into()],
        ..PushoverMessage::default()
    };
    let payload: Value =
        serde_json::from_str(&alert_payload(PushoverChannel::Live, &message)).unwrap();
    assert_eq!(
        payload,
        json!({
            "aps": {
                "alert": {"title": "Destiny is LIVE!", "body": "Debate night"},
                "sound": "default",
                "thread-id": "live:destiny",
                "mutable-content": 1,
                "category": "OMNI_LINK"
            },
            "omni": {
                "channel": "live",
                "url": "https://kick.com/destiny",
                "urlTitle": "Watch on Kick",
                "sender": {
                    "id": "destiny",
                    "name": "Destiny",
                    "avatarUrl": "https://files.kick.com/destiny.webp"
                },
                "refreshWidgets": ["on-air"]
            }
        })
    );
}

#[test]
fn a_plain_message_uses_the_channel_thread_and_plain_category() {
    let message = PushoverMessage {
        message: "Done".into(),
        url_title: Some("ignored without a url".into()),
        ..PushoverMessage::default()
    };
    let payload: Value =
        serde_json::from_str(&alert_payload(PushoverChannel::PressPods, &message)).unwrap();
    assert_eq!(
        payload,
        json!({
            "aps": {
                "alert": {"body": "Done"},
                "sound": "default",
                "thread-id": "presspods",
                "mutable-content": 1,
                "category": "OMNI_PLAIN"
            },
            "omni": {
                "channel": "presspods",
                "url": null,
                "urlTitle": null,
                "sender": null,
                "refreshWidgets": []
            }
        })
    );
}

#[test]
fn truncates_long_bodies_to_fit_the_apns_limit_on_char_boundaries() {
    let body = "é\"".repeat(3_000);
    let message = PushoverMessage {
        message: body.clone(),
        title: Some("Title".into()),
        url: Some("https://example.com".into()),
        ..PushoverMessage::default()
    };
    let encoded = alert_payload(PushoverChannel::General, &message);
    assert!(
        encoded.len() <= MAX_ALERT_PAYLOAD_BYTES,
        "{}",
        encoded.len()
    );
    assert!(
        encoded.len() > MAX_ALERT_PAYLOAD_BYTES - 16,
        "{}",
        encoded.len()
    );
    let payload: Value = serde_json::from_str(&encoded).unwrap();
    let cut = payload["aps"]["alert"]["body"].as_str().unwrap();
    assert!(cut.ends_with('\u{2026}'));
    assert!(body.starts_with(cut.trim_end_matches('\u{2026}')));
    assert_eq!(payload["aps"]["alert"]["title"], "Title");
    assert_eq!(payload["omni"]["url"], "https://example.com");

    // A huge title is cut as well.
    let message = PushoverMessage {
        message: "x".repeat(5_000),
        title: Some("t".repeat(5_000)),
        ..PushoverMessage::default()
    };
    assert!(alert_payload(PushoverChannel::General, &message).len() <= MAX_ALERT_PAYLOAD_BYTES);
}

#[tokio::test(start_paused = true)]
async fn a_push_that_never_left_falls_back_to_pushover() {
    let h = harness(
        Err(ApnsTransportError {
            operation: "provider token creation".into(),
            detail: "invalid key".into(),
            not_sent: true,
        }),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    let outcome = h
        .pushover
        .send(PushoverChannel::General, message())
        .await
        .unwrap();
    assert_eq!(outcome, PushOutcome::Recorded);
    assert_eq!(h.apns.pushes().len(), 1, "a known failure is not retried");
}

#[tokio::test(start_paused = true)]
async fn alerts_expire_after_an_hour_for_live_and_a_day_otherwise() {
    let h = harness(
        Ok(ApnsPushResult::Sent),
        vec![device(
            "iphone-device",
            IosDeviceKind::Iphone,
            Some(token("ab")),
            true,
        )],
    )
    .await;
    h.pushover
        .send_with_token("streamer-app", message())
        .await
        .unwrap();
    h.pushover
        .send(PushoverChannel::Calendar, message())
        .await
        .unwrap();
    let pushes = h.apns.pushes();
    let now = NOW_MS / 1_000;
    assert!((now + 3_600..now + 3_610).contains(&pushes[0].expiration));
    assert!((now + 86_400..now + 86_410).contains(&pushes[1].expiration));
}

#[test]
fn an_empty_body_that_cannot_fit_drops_the_link_instead_of_panicking() {
    let message = PushoverMessage {
        message: String::new(),
        title: None,
        url: Some(format!("https://example.com/{}", "a".repeat(5_000))),
        ..PushoverMessage::default()
    };
    let encoded = alert_payload(PushoverChannel::General, &message);
    assert!(encoded.len() <= MAX_ALERT_PAYLOAD_BYTES);
    let payload: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(payload["omni"]["url"], Value::Null);
    assert_eq!(payload["aps"]["category"], "OMNI_PLAIN");
}
