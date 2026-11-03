//! ERROR-log alerts: layer -> gates -> throttle -> Pushover General.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use omni_alerts::{AlertGate, AlertLayer, Pushover, PushoverChannel};
use omni_core::clock::TestClock;
use omni_http::{HttpClient, HttpConfig, SideEffectMode};
use tracing_subscriber::layer::SubscriberExt as _;

fn recording_pushover() -> Pushover {
    Pushover::with_credentials(
        HttpClient::new(HttpConfig {
            connect_timeout: None,
            offline: true,
        })
        .unwrap(),
        Some("user".to_owned()),
        [(PushoverChannel::General, "general".to_owned())],
        SideEffectMode::Record,
    )
}

struct DenyCastro;

impl AlertGate for DenyCastro {
    fn applies(&self, title: &str) -> bool {
        title.starts_with("Castro")
    }

    fn should_notify<'a>(&'a self, _title: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn chains_the_hook_and_drops_immediate_repeats() {
    let pushover = recording_pushover();
    let gates: Arc<RwLock<Vec<Arc<dyn AlertGate>>>> =
        Arc::new(RwLock::new(vec![Arc::new(DenyCastro)]));
    let (layer, worker) = AlertLayer::new(pushover.clone(), gates, TestClock::new(0));
    let worker = tokio::spawn(worker.run());
    {
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::error!(target: "Test", detail = "first", "Boom");
        tracing::error!(target: "Test", detail = "second", "Boom");
        tracing::error!(target: "Test", "Different boom");
        tracing::warn!(target: "Test", "Only a warning");
        tracing::error!(target: "Castro", "Castro cleanup failed");
        tracing::error!(target: "hyper::proto::h1", "dependency internals");
    }
    // Dropping the subscriber drops the layer's sender, so the worker drains and ends.
    worker.await.unwrap();
    let pushes = pushover.recorded();
    let titles: Vec<_> = pushes
        .iter()
        .map(|p| p.message.title.clone().unwrap_or_default())
        .collect();
    assert_eq!(titles, vec!["Error: Boom", "Error: Different boom"]);
    assert_eq!(pushes[0].message.message, "detail=first");
    assert_eq!(pushes[1].message.message, "Different boom");
    assert!(pushes.iter().all(|p| p.token == "general"));
}

#[tokio::test(flavor = "current_thread")]
async fn alert_pushes_scrub_registered_secrets() {
    omni_core::secrets::register(["alert-sink-secret-42".to_owned()]);
    let pushover = recording_pushover();
    let (layer, worker) = AlertLayer::new(pushover.clone(), Arc::default(), TestClock::new(0));
    let worker = tokio::spawn(worker.run());
    {
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::error!(
            target: "Test",
            detail = "token alert-sink-secret-42",
            "Request alert-sink-secret-42 failed"
        );
    }
    worker.await.unwrap();
    let pushes = pushover.recorded();
    assert_eq!(pushes.len(), 1);
    let title = pushes[0].message.title.clone().unwrap_or_default();
    assert_eq!(title, "Error: Request [redacted] failed");
    assert_eq!(pushes[0].message.message, "detail=token [redacted]");
}
