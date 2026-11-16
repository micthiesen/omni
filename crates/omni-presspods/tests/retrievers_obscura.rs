//! The Obscura retriever: a browser-rendered page is one more candidate in
//! the parallel set, parsed and rated like the fetched ones.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_presspods::error::PressPodsError;
use omni_presspods::retrievers::obscura::{
    MAX_HTML_UNITS, ObscuraRetriever, PageRenderer, RENDER_TIMEOUT, RenderedPage, usable_html,
};
use omni_presspods::retrievers::{
    ArticleRetriever, BestArticle, RETRIEVER_TIMEOUT, RetrieverContext, article_retrievers,
    rate_retrieved_articles, run_article_retrievers, select_best,
};
use omni_presspods::types::{Article, MetadataInfo, RetrieverResult};
use omni_testkit::{TEST_EPOCH_MS, TestStore, mock_http, test_clock};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const URL: &str = "https://news.test/story";
const MARKER: &str = "The reporter then describes what the council decided";

/// A page whose article is in the markup.
fn article_page() -> String {
    let paragraph = format!(
        "<p>{MARKER} and why the vote surprised the people who had followed the debate.</p>"
    );
    format!(
        "<html><head><title>Council vote</title></head><body><nav><a href=\"/\">Home</a></nav>\
         <article><h1>Council vote</h1>{}</article><footer>Contact</footer></body></html>",
        paragraph.repeat(8)
    )
}

/// What a plain fetch gets from a page that builds its article with scripts.
fn script_shell() -> String {
    format!(
        "<html><head><title>Council vote</title></head><body><div id=\"root\"></div>\
         <script>{}</script></body></html>",
        "window.__boot = window.__boot || [];".repeat(20)
    )
}

fn rendered(html: String) -> RenderedPage {
    RenderedPage {
        href: URL.to_owned(),
        content_type: "text/html".to_owned(),
        chars: html.encode_utf16().count(),
        html: Some(html),
        challenge: false,
        paragraph_chars: 2_000,
    }
}

enum Renders {
    Page(RenderedPage),
    Fails(&'static str),
    Hangs,
}

struct FakeRenderer {
    renders: Renders,
    urls: Mutex<Vec<String>>,
}

impl FakeRenderer {
    fn new(renders: Renders) -> Arc<Self> {
        Arc::new(Self {
            renders,
            urls: Mutex::default(),
        })
    }
}

impl PageRenderer for FakeRenderer {
    fn render<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<RenderedPage, PressPodsError>> {
        Box::pin(async move {
            self.urls.lock().unwrap().push(url.to_owned());
            match &self.renders {
                Renders::Page(page) => Ok(page.clone()),
                Renders::Fails(message) => Err(PressPodsError::failed(
                    "render article with Obscura",
                    *message,
                )),
                Renders::Hangs => std::future::pending().await,
            }
        })
    }
}

/// A context whose fetches of `news.test` and the proxy services go to `server`.
async fn context(
    server: &MockServer,
    renderer: Option<Arc<FakeRenderer>>,
) -> (Arc<RetrieverContext>, TestStore) {
    let clock: omni_core::clock::SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let http = mock_http(
        server,
        &[
            "https://news.test",
            "https://archive.org",
            "https://www.removepaywall.com",
        ],
    );
    let ctx = Arc::new(RetrieverContext {
        public_http: omni_http::public::PublicHttpClient::new(&http).allow_loopback_for_tests(),
        http,
        jina_api_key: None,
        renderer: renderer.map(|r| r as Arc<dyn PageRenderer>),
        costs: omni_ai::costs::CostRecorder::new(store.store.clone(), clock),
        tz: jiff::tz::TimeZone::UTC,
    });
    (ctx, store)
}

async fn serve_story(server: &MockServer, html: String) {
    Mock::given(method("GET"))
        .and(path("/story"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.into_bytes(), "text/html"))
        .mount(server)
        .await;
}

fn metadata(rating: f64) -> MetadataInfo {
    MetadataInfo {
        is_valid_article: true,
        title: None,
        author: None,
        author_gender: None,
        coauthors: None,
        publication: None,
        published_at: None,
        lead_image_url: None,
        short_summary: None,
        content_rating: rating,
    }
}

/// Retrieves and rates like the pipeline, with a rating that favors
/// extractions holding the article text. Returns the pick and how many
/// distinct extractions were rated.
async fn pick(ctx: &Arc<RetrieverContext>) -> (Result<BestArticle, PressPodsError>, usize) {
    let retrievers = article_retrievers(ctx, URL);
    let retrieved = run_article_retrievers(URL, &retrievers, RETRIEVER_TIMEOUT).await;
    let rated = Arc::new(AtomicUsize::new(0));
    let counter = rated.clone();
    let results = rate_retrieved_articles(retrieved, move |article: Article| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move {
            Ok(metadata(if article.text.contains(MARKER) {
                9.0
            } else {
                2.0
            }))
        }
    })
    .await;
    (select_best(results), rated.load(Ordering::SeqCst))
}

fn failure<'a>(best: &'a BestArticle, name: &str) -> &'a PressPodsError {
    best.all_results
        .iter()
        .find_map(|r| match r {
            RetrieverResult::Failure {
                retriever_name,
                error,
            } if retriever_name == name => Some(error),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} should have failed"))
}

#[tokio::test]
async fn a_script_built_page_is_won_by_the_rendered_candidate() {
    let server = MockServer::start().await;
    serve_story(&server, script_shell()).await;
    let renderer = FakeRenderer::new(Renders::Page(rendered(article_page())));
    let (ctx, _store) = context(&server, Some(renderer.clone())).await;

    let (best, _) = pick(&ctx).await;
    let best = best.unwrap();

    assert_eq!(best.retriever_name, "obscura");
    assert!(best.article.text.contains(MARKER));
    assert_eq!(best.article.title.as_deref(), Some("Council vote"));
    assert_eq!(best.article.url, URL);
    assert_eq!(*renderer.urls.lock().unwrap(), [URL]);
    // Every candidate's outcome is kept for the episode's attempt list.
    let names: Vec<&str> = best
        .all_results
        .iter()
        .map(RetrieverResult::retriever_name)
        .collect();
    assert_eq!(names.last(), Some(&"obscura"));
    assert!(names.contains(&"postlight") && names.contains(&"fetch"));
}

#[tokio::test]
async fn an_identical_rendered_extraction_is_rated_once_and_the_fetched_one_wins_the_tie() {
    let server = MockServer::start().await;
    serve_story(&server, article_page()).await;
    let with = FakeRenderer::new(Renders::Page(rendered(article_page())));
    let (ctx, _store) = context(&server, Some(with)).await;
    let (best, rated_with) = pick(&ctx).await;
    let best = best.unwrap();

    let (ctx_without, _store) = context(&server, None).await;
    let (_, rated_without) = pick(&ctx_without).await;

    assert_eq!(best.retriever_name, "postlight");
    assert!(best.all_results.iter().any(|r| matches!(r,
        RetrieverResult::Success { retriever_name, metadata, .. }
            if retriever_name == "obscura" && metadata.content_rating == 9.0)));
    // The browser's render added nothing new, so it cost no extra rating.
    assert_eq!(rated_with, rated_without);
}

#[tokio::test]
async fn a_failed_render_is_one_failed_candidate_and_scoring_proceeds() {
    let server = MockServer::start().await;
    serve_story(&server, article_page()).await;
    let renderer = FakeRenderer::new(Renders::Fails("could not start obscura: not found"));
    let (ctx, _store) = context(&server, Some(renderer)).await;

    let (best, _) = pick(&ctx).await;
    let best = best.unwrap();

    assert_eq!(best.retriever_name, "postlight");
    assert_eq!(
        failure(&best, "obscura").to_string(),
        "render article with Obscura: could not start obscura: not found"
    );
}

#[tokio::test]
async fn without_a_renderer_the_set_has_no_obscura_candidate() {
    let server = MockServer::start().await;
    let (ctx, _store) = context(&server, None).await;
    let names: Vec<String> = article_retrievers(&ctx, URL)
        .iter()
        .map(|r| r.name().to_owned())
        .collect();
    assert!(!names.contains(&"obscura".to_owned()), "{names:?}");

    let renderer = FakeRenderer::new(Renders::Hangs);
    let (ctx, _store) = context(&server, Some(renderer)).await;
    let x: Vec<String> = article_retrievers(&ctx, "https://x.com/i/status/2079904005652893709")
        .iter()
        .map(|r| r.name().to_owned())
        .collect();
    assert_eq!(x, ["x"]);
}

#[tokio::test(start_paused = true)]
async fn a_hung_browser_fails_at_its_own_bound_inside_the_retriever_budget() {
    let server = MockServer::start().await;
    let (ctx, _store) = context(&server, None).await;
    let retriever = ObscuraRetriever {
        ctx,
        renderer: FakeRenderer::new(Renders::Hangs),
    };
    let started = tokio::time::Instant::now();
    let error = retriever.retrieve(URL, "ignored").await.unwrap_err();
    assert_eq!(started.elapsed(), RENDER_TIMEOUT);
    assert!(RENDER_TIMEOUT < RETRIEVER_TIMEOUT);
    assert_eq!(
        error.to_string(),
        "render article with Obscura: no rendered page within 55 seconds"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_non_public_url_never_reaches_the_browser() {
    let server = MockServer::start().await;
    let (ctx, _store) = context(&server, None).await;
    let renderer = FakeRenderer::new(Renders::Page(rendered(article_page())));
    let retriever = ObscuraRetriever {
        ctx,
        renderer: renderer.clone(),
    };
    for url in ["http://127.0.0.1/admin", "file:///etc/passwd"] {
        assert!(retriever.retrieve(url, "ignored").await.is_err(), "{url}");
    }
    assert!(renderer.urls.lock().unwrap().is_empty());
}

#[test]
fn unusable_renders_say_why() {
    let message = |page: RenderedPage| usable_html(page).unwrap_err().to_string();
    let page = || rendered(article_page());

    assert_eq!(
        message(RenderedPage {
            challenge: true,
            paragraph_chars: 0,
            ..page()
        }),
        "render article with Obscura: the site answered the browser with a bot challenge"
    );
    assert_eq!(
        message(RenderedPage {
            html: None,
            chars: MAX_HTML_UNITS + 1,
            ..page()
        }),
        "render article with Obscura: the rendered page is too large \
         (5000001 characters, limit 5000000)"
    );
    assert_eq!(
        message(RenderedPage {
            content_type: "application/pdf".into(),
            ..page()
        }),
        "render article with Obscura: the page is application/pdf, not HTML"
    );
    assert!(
        message(RenderedPage {
            href: "http://192.168.1.10/router".into(),
            ..page()
        })
        .starts_with("render article with Obscura: the page redirected off the public web"),
    );
    // An article that merely embeds a challenge widget is still read.
    assert!(
        usable_html(RenderedPage {
            challenge: true,
            ..page()
        })
        .is_ok()
    );
}

#[test]
fn render_bound_leaves_room_before_the_shared_retriever_deadline() {
    assert!(RETRIEVER_TIMEOUT - RENDER_TIMEOUT >= Duration::from_secs(5));
}
