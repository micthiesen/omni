//! The `PageFetcher` port: every retriever's candidate for a public page,
//! unrated, with the page HTML from the sources that hold it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use futures::future::BoxFuture;
use omni_ai::costs::CostRecorder;
use omni_presspods::error::PressPodsError;
use omni_presspods::page_fetch::PressPodsPageFetcher;
use omni_presspods::retrievers::RetrieverContext;
use omni_presspods::retrievers::obscura::{PageRenderer, RenderedPage};
use omni_runtime::ports::{FetchedPage, PageFetcher, PortError};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, test_clock};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const URL: &str = "https://kitchen.test/lemon-cake";
const SNAPSHOT: &str = "http://web.archive.org/web/20260714070431/https://kitchen.test/lemon-cake";
const JSON_LD: &str = r#"{"@context":"https://schema.org","@type":"Recipe","name":"Lemon cake","recipeIngredient":["200 g flour","2 lemons"]}"#;

fn recipe_page(marker: &str) -> String {
    let paragraph = format!(
        "<p>{marker}: beat the butter with the sugar until pale, then fold in the flour \
         and the zest of both lemons before the tin goes into the oven.</p>"
    );
    format!(
        "<html><head><title>Lemon cake</title>\
         <meta property=\"og:image\" content=\"https://kitchen.test/cake.jpg\">\
         <meta name=\"author\" content=\"Ann Baker\">\
         <script type=\"application/ld+json\">{JSON_LD}</script></head>\
         <body><nav><a href=\"/\">Home</a></nav><article><h1>Lemon cake</h1>{}</article>\
         <footer>Contact</footer></body></html>",
        paragraph.repeat(8)
    )
}

struct FixedRender(String);

impl PageRenderer for FixedRender {
    fn render<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<RenderedPage, PressPodsError>> {
        let html = self.0.clone();
        Box::pin(async move {
            Ok(RenderedPage {
                href: "https://kitchen.test/recipes/lemon-cake".to_owned(),
                content_type: "text/html".to_owned(),
                chars: html.encode_utf16().count(),
                html: Some(html),
                challenge: false,
                paragraph_chars: 2_000,
            })
        })
    }
}

async fn fetcher(
    server: &MockServer,
    renderer: Option<Arc<dyn PageRenderer>>,
) -> (PressPodsPageFetcher, TestStore) {
    let clock: omni_core::clock::SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let http = mock_http(
        server,
        &[
            "https://kitchen.test",
            "https://archive.org",
            "http://web.archive.org",
        ],
    );
    let ctx = Arc::new(RetrieverContext {
        public_http: omni_http::public::PublicHttpClient::new(&http).allow_loopback_for_tests(),
        http,
        jina_api_key: None,
        renderer,
        costs: CostRecorder::new(store.store.clone(), clock),
        tz: jiff::tz::TimeZone::UTC,
    });
    (PressPodsPageFetcher::new(ctx), store)
}

async fn mount_page(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path("/lemon-cake"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn mount_wayback(server: &MockServer, snapshot: Option<&str>) {
    let closest = snapshot.map(|_| {
        json!({"status": "200", "available": true, "url": SNAPSHOT, "timestamp": "20260714070431"})
    });
    Mock::given(method("GET"))
        .and(path("/wayback/available"))
        .and(query_param("url", URL))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": URL,
            "archived_snapshots": closest.map_or(json!({}), |c| json!({"closest": c})),
        })))
        .mount(server)
        .await;
    if let Some(html) = snapshot {
        Mock::given(method("GET"))
            .and(path("/web/20260714070431/https://kitchen.test/lemon-cake"))
            .respond_with(ResponseTemplate::new(200).set_body_string(html))
            .mount(server)
            .await;
    }
}

fn from<'a>(pages: &'a [FetchedPage], source: &str) -> &'a FetchedPage {
    pages
        .iter()
        .find(|page| page.source == source)
        .unwrap_or_else(|| panic!("no {source} page"))
}

fn sources(pages: &[FetchedPage]) -> Vec<&str> {
    pages.iter().map(|page| page.source.as_str()).collect()
}

#[tokio::test]
async fn a_page_comes_back_with_its_html_and_each_extraction() {
    let server = MockServer::start().await;
    let page = recipe_page("Direct");
    mount_page(&server, ResponseTemplate::new(200).set_body_string(&page)).await;
    mount_wayback(&server, None).await;
    let (fetcher, _store) = fetcher(&server, None).await;

    let pages = fetcher.fetch_pages(URL).await.unwrap();
    assert_eq!(
        sources(&pages),
        ["postlight", "readability", "extractus", "fetch"]
    );

    // The direct fetch carries the page as served, structured data included.
    let fetch = from(&pages, "fetch");
    assert_eq!(fetch.html.as_deref(), Some(page.as_str()));
    assert!(fetch.html.as_deref().unwrap().contains(JSON_LD));
    assert_eq!(fetch.url, URL);
    assert_eq!(fetch.title.as_deref(), Some("Lemon cake"));

    // The extractors carry readable text and metadata, not the page.
    let readability = from(&pages, "readability");
    assert_eq!(readability.html, None);
    let text = readability.text.as_deref().unwrap();
    assert!(text.contains("fold in the flour"), "{text}");
    assert!(!text.contains("<p>"), "{text}");
    assert_eq!(
        readability.lead_image_url.as_deref(),
        Some("https://kitchen.test/cake.jpg")
    );
    assert_eq!(
        from(&pages, "postlight").author.as_deref(),
        Some("Ann Baker")
    );
}

#[tokio::test]
async fn a_refused_page_can_still_come_from_the_archive_and_the_browser() {
    let server = MockServer::start().await;
    mount_page(
        &server,
        ResponseTemplate::new(403).set_body_string("SECRET CHALLENGE PAGE"),
    )
    .await;
    let archived = recipe_page("Archived");
    mount_wayback(&server, Some(&archived)).await;
    let rendered = recipe_page("Rendered");
    let (fetcher, _store) = fetcher(&server, Some(Arc::new(FixedRender(rendered.clone())))).await;

    let pages = fetcher.fetch_pages(URL).await.unwrap();
    assert_eq!(sources(&pages), ["wayback", "obscura"]);
    let wayback = from(&pages, "wayback");
    assert_eq!(wayback.html.as_deref(), Some(archived.as_str()));
    assert_eq!(wayback.url, SNAPSHOT);
    assert!(wayback.text.as_deref().unwrap().contains("Archived"));
    let obscura = from(&pages, "obscura");
    assert_eq!(obscura.html.as_deref(), Some(rendered.as_str()));
    assert_eq!(obscura.url, "https://kitchen.test/recipes/lemon-cake");
    assert!(obscura.text.as_deref().unwrap().contains("Rendered"));
}

#[tokio::test]
async fn a_page_no_source_has_says_what_the_host_did() {
    let server = MockServer::start().await;
    mount_page(
        &server,
        ResponseTemplate::new(403).set_body_string("SECRET CHALLENGE PAGE"),
    )
    .await;
    mount_wayback(&server, None).await;
    let (fetcher, _store) = fetcher(&server, None).await;

    let error = fetcher.fetch_pages(URL).await.unwrap_err();
    let PortError::Failed { message, transient } = error else {
        panic!("{error}");
    };
    assert_eq!(
        message,
        "The host refused the request (HTTP 403); all page sources failed \
         (postlight, readability, extractus, fetch: HTTP 403; \
         wayback: No archived snapshot available for this URL)"
    );
    assert!(!transient);
}

#[tokio::test]
async fn a_url_off_the_public_web_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let (fetcher, _store) = fetcher(&server, None).await;
    for url in [
        "http://127.0.0.1/admin",
        "http://localhost/recipe",
        "file:///etc/passwd",
        "https://user:pw@kitchen.test/lemon-cake",
        "not a url",
    ] {
        let error = fetcher.fetch_pages(url).await.unwrap_err();
        assert!(
            matches!(&error, PortError::Failed { message, transient: false }
                if message.starts_with("Not a public http(s) URL")),
            "{url}: {error}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
