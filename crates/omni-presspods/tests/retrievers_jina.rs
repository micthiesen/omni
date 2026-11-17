//! The Jina retriever against a local mock of the
//! Jina Reader API.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use omni_ai::costs::{CostEventData, CostRecorder};
use omni_api::costs::{CostPriceStatus, CostUsage};
use omni_presspods::retrievers::proxies::{JINA_READER_CENTS_PER_TOKEN, JinaRetriever};
use omni_presspods::retrievers::{ArticleRetriever, RetrieverContext};
use omni_store::EntityOps;
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, test_clock};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn html() -> String {
    format!(
        "<html><head><title>Fallback title</title></head><body><article>{}</article></body></html>",
        "Article text. ".repeat(20)
    )
}

async fn retriever(server: &MockServer) -> (JinaRetriever, TestStore) {
    let clock: omni_core::clock::SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let http = mock_http(server, &["https://r.jina.ai"]);
    let ctx = Arc::new(RetrieverContext {
        public_http: omni_http::public::PublicHttpClient::new(&http).allow_loopback_for_tests(),
        http,
        jina_api_key: Some("test-key".into()),
        renderer: None,
        costs: CostRecorder::new(store.store.clone(), clock),
        tz: jiff::tz::TimeZone::UTC,
    });
    (
        JinaRetriever {
            ctx,
            api_key: "test-key".into(),
        },
        store,
    )
}

async fn events(store: &TestStore) -> Vec<CostEventData> {
    store
        .store
        .read(|docs| docs.get_all::<CostEventData>())
        .await
        .unwrap()
}

#[tokio::test]
async fn requests_json_usage_and_records_an_estimated_token_cost() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/https://example.com/story"))
        .and(header("accept", "application/json"))
        .and(header("x-respond-with", "html"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "title": "Jina title", "content": html(), "usage": { "tokens": 1234 } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (jina, store) = retriever(&server).await;
    let article = jina
        .retrieve("https://example.com/story", "ignored")
        .await
        .unwrap();
    assert_eq!(article.title.as_deref(), Some("Jina title"));
    assert_eq!(article.domain.as_deref(), Some("example.com"));
    assert_eq!(article.url, "https://example.com/story");
    let events = events(&store).await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.service, "jina");
    assert_eq!(event.model.as_deref(), Some("reader"));
    assert_eq!(event.cost_cents, Some(1234.0 * JINA_READER_CENTS_PER_TOKEN));
    assert_eq!(event.price_status, CostPriceStatus::Estimated);
    assert_eq!(
        event.usage,
        CostUsage {
            requests: Some(1.0),
            output_tokens: Some(1234.0),
            ..CostUsage::default()
        }
    );
}

#[tokio::test]
async fn keeps_missing_usage_explicitly_unpriced_instead_of_inventing_a_cost() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": { "content": html() } })),
        )
        .mount(&server)
        .await;
    let (jina, store) = retriever(&server).await;
    let article = jina
        .retrieve("https://example.com/story", "ignored")
        .await
        .unwrap();
    assert_eq!(article.title.as_deref(), Some("Fallback title"));
    let events = events(&store).await;
    assert_eq!(events[0].cost_cents, None);
    assert_eq!(events[0].price_status, CostPriceStatus::Unknown);
    assert_eq!(
        events[0].usage,
        CostUsage {
            requests: Some(1.0),
            ..CostUsage::default()
        }
    );
}

#[tokio::test]
async fn reads_the_page_from_the_html_field_the_reader_returns_for_html_responses() {
    let server = MockServer::start().await;
    // The shape r.jina.ai returned on 2026-11-16 for `x-respond-with: html`:
    // the page is in `data.html` and there is no `data.content`.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 200,
            "data": {
                "title": "Jina title",
                "url": "https://example.com/story",
                "html": html(),
                "usage": { "tokens": 10 }
            }
        })))
        .mount(&server)
        .await;
    let (jina, _store) = retriever(&server).await;
    let article = jina
        .retrieve("https://example.com/story", "ignored")
        .await
        .unwrap();
    assert!(article.text.contains("Article text."), "{}", article.text);
    assert_eq!(article.title.as_deref(), Some("Jina title"));
}

#[tokio::test]
async fn a_response_without_page_html_is_a_typed_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": { "title": "Only a title" } })),
        )
        .mount(&server)
        .await;
    let (jina, store) = retriever(&server).await;
    let error = jina
        .retrieve("https://example.com/story", "ignored")
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "retrieve article with Jina: Jina returned empty or too short content"
    );
    assert!(events(&store).await.is_empty());
}
