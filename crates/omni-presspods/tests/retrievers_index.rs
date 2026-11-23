//! Running the retrievers and choosing the best article.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_presspods::error::PressPodsError;
use omni_presspods::retrievers::{
    ArticleRetriever, Retrieved, RetrieverContext, article_retrievers, rate_retrieved_articles,
    run_article_retrievers,
};
use omni_presspods::types::{Article, MetadataInfo, RetrieverResult};

fn article(text: &str, title: &str) -> Article {
    Article {
        title: Some(title.into()),
        text: text.into(),
        author: None,
        domain: Some("example.com".into()),
        url: format!("https://example.com/{title}"),
        published_at: None,
        lead_image_url: None,
    }
}

fn metadata(valid: bool, rating: f64) -> MetadataInfo {
    MetadataInfo {
        is_valid_article: valid,
        title: Some("Rated title".into()),
        author: None,
        author_gender: Some(omni_presspods::model::AuthorGender::Unknown),
        coauthors: None,
        publication: Some("Example".into()),
        published_at: None,
        lead_image_url: None,
        short_summary: Some("Summary".into()),
        content_rating: rating,
    }
}

fn ok(name: &str, a: Article) -> Retrieved {
    Retrieved::Success {
        retriever_name: name.into(),
        article: a,
    }
}

fn names(results: &[RetrieverResult]) -> Vec<&str> {
    results
        .iter()
        .map(RetrieverResult::retriever_name)
        .collect()
}

#[tokio::test]
async fn rates_normalized_exact_text_once_and_preserves_provider_order_and_articles() {
    let first = article("same\r\ntext\n", "first");
    let second = Article {
        text: " same\ntext".into(),
        ..first.clone()
    };
    let distinct = article("same text", "distinct");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorder = calls.clone();
    let results = rate_retrieved_articles(
        vec![
            ok("first", first.clone()),
            Retrieved::Failure {
                retriever_name: "failed".into(),
                error: PressPodsError::failed("retrieve", "network"),
            },
            ok("second", second.clone()),
            ok("distinct", distinct.clone()),
        ],
        move |a: Article| {
            recorder.lock().unwrap().push(a);
            async { Ok(metadata(true, 9.0)) }
        },
    )
    .await;
    assert_eq!(
        *calls.lock().unwrap(),
        vec![first.clone(), distinct.clone()]
    );
    assert_eq!(names(&results), ["first", "failed", "second", "distinct"]);
    assert!(matches!(&results[0], RetrieverResult::Success { article, .. } if *article == first));
    assert!(matches!(&results[1], RetrieverResult::Failure { .. }));
    assert!(matches!(&results[2], RetrieverResult::Success { article, .. } if *article == second));
    assert!(
        matches!(&results[3], RetrieverResult::Success { article, .. } if *article == distinct)
    );
}

#[tokio::test]
async fn rates_matching_body_text_separately_when_prompt_metadata_differs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    rate_retrieved_articles(
        vec![
            ok("first", article("same text", "first")),
            ok("second", article("same text", "second")),
        ],
        move |_a: Article| {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Ok(metadata(true, 9.0)) }
        },
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn fans_invalid_metadata_and_rating_errors_out_to_each_matching_retriever() {
    let invalid = article("invalid", "invalid-a");
    let broken = article("broken", "broken-a");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let results = rate_retrieved_articles(
        vec![
            ok("invalid-a", invalid.clone()),
            ok("broken-a", broken.clone()),
            ok("invalid-b", invalid.clone()),
            ok("broken-b", broken.clone()),
        ],
        move |a: Article| {
            counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if a.text == "broken" {
                    Err(PressPodsError::failed("rate article", "model unavailable"))
                } else {
                    Ok(metadata(false, 9.0))
                }
            }
        },
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(results.len(), 4);
    let messages: Vec<String> = results
        .iter()
        .map(|r| match r {
            RetrieverResult::Failure { error, .. } => error.cause_message(),
            RetrieverResult::Success { .. } => panic!("expected failures"),
        })
        .collect();
    assert_eq!(
        messages,
        [
            "Invalid article",
            "model unavailable",
            "Invalid article",
            "model unavailable"
        ]
    );
}

async fn context() -> Arc<RetrieverContext> {
    let clock: omni_core::clock::SharedClock =
        omni_testkit::test_clock(omni_testkit::TEST_EPOCH_MS);
    let http = omni_testkit::no_network();
    let store = omni_testkit::TestStore::new(clock.clone()).await;
    Arc::new(RetrieverContext {
        public_http: omni_http::public::PublicHttpClient::new(&http),
        http,
        jina_api_key: None,
        renderer: None,
        costs: omni_ai::costs::CostRecorder::new(store.store.clone(), clock),
        tz: jiff::tz::TimeZone::UTC,
    })
}

async fn retriever_names(url: &str) -> Vec<String> {
    article_retrievers(&context().await, url)
        .iter()
        .map(|r| r.name().to_owned())
        .collect()
}

#[tokio::test]
async fn uses_the_specialized_retriever_for_x_and_twitter_status_urls() {
    for url in [
        "https://x.com/edels0n/status/2077031491045929255?s=46&t=share",
        "https://mobile.twitter.com/user/status/2079904005652893709",
        "https://x.com/i/status/2079904005652893709",
        "https://x.com/i/web/status/2079904005652893709",
    ] {
        assert_eq!(retriever_names(url).await, ["x"], "{url}");
    }
}

#[tokio::test]
async fn keeps_generic_retrievers_for_non_status_and_lookalike_urls() {
    assert!(
        retriever_names("https://x.com/explore")
            .await
            .contains(&"readability".to_owned())
    );
    assert!(
        retriever_names("https://x.com.example/status/123")
            .await
            .contains(&"readability".to_owned())
    );
    assert_eq!(
        retriever_names("https://example.com/a").await,
        ["postlight", "readability", "extractus", "wayback", "fetch"]
    );
}

struct Hung(Arc<AtomicBool>);

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl ArticleRetriever for Hung {
    fn name(&self) -> &str {
        "hung"
    }
    fn retrieve<'a>(
        &'a self,
        _url: &'a str,
        _ua: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        let flag = DropFlag(self.0.clone());
        Box::pin(async move {
            let _flag = flag;
            futures::future::pending::<()>().await;
            unreachable!()
        })
    }
}

#[tokio::test(start_paused = true)]
async fn interrupts_a_timed_out_child_without_discarding_a_successful_sibling() {
    let interrupted = Arc::new(AtomicBool::new(false));
    let successful = article("complete", "successful");
    let retrievers: Vec<Arc<dyn ArticleRetriever>> = vec![
        Arc::new(Hung(interrupted.clone())),
        Arc::new(common::FakeRetriever {
            name: "successful",
            result: Ok(successful.clone()),
        }),
    ];
    let results = run_article_retrievers(
        "https://example.com/article",
        &retrievers,
        Duration::from_millis(5),
    )
    .await;
    assert!(interrupted.load(Ordering::SeqCst));
    assert_eq!(results.len(), 2);
    assert!(
        matches!(&results[0], Retrieved::Failure { retriever_name, .. } if retriever_name == "hung")
    );
    assert!(
        matches!(&results[1], Retrieved::Success { retriever_name, article } if retriever_name == "successful" && *article == successful)
    );
}

#[test]
fn selects_the_highest_rating_and_the_first_on_ties() {
    let results = vec![
        RetrieverResult::Success {
            retriever_name: "a".into(),
            article: article("a", "a"),
            metadata: metadata(true, 7.0),
        },
        RetrieverResult::Success {
            retriever_name: "b".into(),
            article: article("b", "b"),
            metadata: metadata(true, 9.0),
        },
        RetrieverResult::Success {
            retriever_name: "c".into(),
            article: article("c", "c"),
            metadata: metadata(true, 9.0),
        },
    ];
    assert_eq!(
        omni_presspods::retrievers::select_best(results)
            .unwrap()
            .retriever_name,
        "b"
    );
    let error = omni_presspods::retrievers::select_best(vec![RetrieverResult::Failure {
        retriever_name: "a".into(),
        error: PressPodsError::failed("x", "y"),
        text_chars: None,
    }])
    .unwrap_err();
    assert_eq!(
        error.cause_message(),
        "All article retrievers failed (a: y)"
    );
    assert!(!error.is_retryable());
}

fn failed_rating(name: &str, source: PressPodsError) -> RetrieverResult {
    RetrieverResult::Failure {
        retriever_name: name.into(),
        error: PressPodsError::Failed {
            operation: omni_presspods::retrievers::RATING_OPERATION.into(),
            message: source.cause_message(),
            retryable: Some(source.is_retryable()),
        },
        text_chars: None,
    }
}

#[test]
fn the_failure_names_each_retrievers_reason_without_response_bodies() {
    let blocked = || {
        PressPodsError::http(
            "fetch public PressPods HTML",
            omni_http::HttpError::Status {
                status: 401,
                body: "<html><body>Please enable JS and disable any ad blocker</body></html>"
                    .into(),
            },
        )
    };
    let error = omni_presspods::retrievers::select_best(vec![
        RetrieverResult::Failure {
            retriever_name: "postlight".into(),
            error: blocked(),
            text_chars: None,
        },
        RetrieverResult::Failure {
            retriever_name: "wayback".into(),
            error: PressPodsError::failed(
                "retrieve article with Wayback",
                "No archived snapshot available for this URL",
            ),
            text_chars: None,
        },
        failed_rating("fetch", PressPodsError::failed("rate", "Invalid article")),
        RetrieverResult::Failure {
            retriever_name: "obscura".into(),
            error: PressPodsError::failed(
                "render article with Obscura",
                "the site answered the browser with a bot challenge",
            ),
            text_chars: None,
        },
    ])
    .unwrap_err();
    assert_eq!(
        error.summary(),
        "Error: All article retrievers failed (postlight: HTTP 401; \
         wayback: No archived snapshot available for this URL; fetch: Invalid article; \
         obscura: the site answered the browser with a bot challenge)"
    );
    assert!(!error.is_retryable());
}

#[test]
fn eight_distinct_reasons_all_fit_in_the_stored_summary() {
    let names = [
        "postlight",
        "readability",
        "extractus",
        "wayback",
        "fetch",
        "jina",
        "obscura",
        "x",
    ];
    let failures = names
        .iter()
        .enumerate()
        .map(|(i, name)| RetrieverResult::Failure {
            retriever_name: (*name).into(),
            error: PressPodsError::failed("retrieve", format!("{i}{}", "y".repeat(90))),
            text_chars: None,
        })
        .collect();
    let summary = omni_presspods::retrievers::select_best(failures)
        .unwrap_err()
        .summary();
    assert!(summary.contains("obscura: 6yyy"), "{summary}");
    assert!(summary.ends_with("...)"), "{summary}");
}

#[test]
fn retrievers_that_failed_the_same_way_share_one_reason() {
    let blocked = || PressPodsError::failed("fetch", "HTTP 403");
    let error = omni_presspods::retrievers::select_best(vec![
        RetrieverResult::Failure {
            retriever_name: "postlight".into(),
            error: blocked(),
            text_chars: None,
        },
        RetrieverResult::Failure {
            retriever_name: "wayback".into(),
            error: PressPodsError::failed("retrieve", "No snapshot"),
            text_chars: None,
        },
        RetrieverResult::Failure {
            retriever_name: "fetch".into(),
            error: blocked(),
            text_chars: None,
        },
    ])
    .unwrap_err();
    assert_eq!(
        error.cause_message(),
        "All article retrievers failed (postlight, fetch: HTTP 403; wayback: No snapshot)"
    );
}

#[test]
fn a_rating_model_outage_is_retried_but_a_rejected_article_is_not() {
    let outage = PressPodsError::status("generate PressPods article metadata", 503, "overloaded");
    let error = omni_presspods::retrievers::select_best(vec![
        RetrieverResult::Failure {
            retriever_name: "wayback".into(),
            error: PressPodsError::failed("retrieve", "No archived snapshot"),
            text_chars: None,
        },
        failed_rating("postlight", outage),
    ])
    .unwrap_err();
    assert!(error.is_retryable());

    let rejected = omni_presspods::retrievers::select_best(vec![failed_rating(
        "postlight",
        PressPodsError::failed("rate", "Invalid article"),
    )])
    .unwrap_err();
    assert!(!rejected.is_retryable());
}

#[test]
fn long_failure_reasons_are_cut_to_their_first_line() {
    let reason = omni_presspods::types::failure_reason(&PressPodsError::failed(
        "decode",
        format!("{}\nsecond line", "x".repeat(80)),
    ));
    assert_eq!(reason, format!("{}...", "x".repeat(57)));
}
