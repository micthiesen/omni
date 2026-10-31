//! Amazon reads clear Amazon's bot check by warming up on the home page,
//! stop on an image CAPTCHA or a missing product, and replace a failed browser.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_personal::amazon::browser::{Browser, Launcher};
use omni_personal::amazon::page::{
    Asin, CONTINUE_SCRIPT, PRODUCT_SCRIPT, SEARCH_SCRIPT, STATE_SCRIPT, SearchOptions, Sort, Store,
};
use omni_personal::amazon::{AmazonError, AmazonService};
use serde_json::json;

#[derive(Default)]
struct Script {
    /// Page states returned by successive state probes.
    states: VecDeque<serde_json::Value>,
    extract: String,
    /// Fail the next navigation.
    fail_navigation: bool,
    /// Never answer the next navigation.
    hang_navigation: bool,
    log: Vec<String>,
    launches: u32,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Script>>);

impl Fake {
    fn with<R>(&self, f: impl FnOnce(&mut Script) -> R) -> R {
        f(&mut self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn log(&self) -> Vec<String> {
        self.with(|s| s.log.clone())
    }
}

struct FakeBrowser(Fake);

impl Browser for FakeBrowser {
    fn navigate<'a>(&'a mut self, url: &'a str) -> BoxFuture<'a, Result<(), AmazonError>> {
        let hang = self.0.with(|s| std::mem::take(&mut s.hang_navigation));
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            self.0.with(|s| {
                s.log.push(format!("goto {url}"));
                if std::mem::take(&mut s.fail_navigation) {
                    return Err(AmazonError::Browser("the browser exited".into()));
                }
                Ok(())
            })
        })
    }

    fn evaluate<'a>(
        &'a mut self,
        expression: &'a str,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<String, AmazonError>> {
        Box::pin(async move {
            self.0.with(|s| {
                let out = if expression == STATE_SCRIPT {
                    s.log.push("state".into());
                    s.states
                        .pop_front()
                        .unwrap_or_else(|| json!({}))
                        .to_string()
                } else if expression == CONTINUE_SCRIPT {
                    s.log.push("continue".into());
                    "true".into()
                } else if expression == SEARCH_SCRIPT || expression == PRODUCT_SCRIPT {
                    s.log.push("extract".into());
                    s.extract.clone()
                } else {
                    s.log.push("wait".into());
                    "true".into()
                };
                Ok(out)
            })
        })
    }
}

impl Launcher for Fake {
    fn launch(&self) -> BoxFuture<'_, Result<Box<dyn Browser>, AmazonError>> {
        Box::pin(async move {
            self.with(|s| s.launches += 1);
            Ok(Box::new(FakeBrowser(self.clone())) as Box<dyn Browser>)
        })
    }
}

fn service(fake: &Fake) -> AmazonService {
    AmazonService::with_settle(Arc::new(fake.clone()), Duration::from_millis(1))
}

fn options() -> SearchOptions {
    SearchOptions {
        store: Store::Ca,
        page: 1,
        limit: 20,
        include_sponsored: false,
    }
}

#[tokio::test(start_paused = true)]
async fn search_warms_up_through_both_bot_checks_then_reads_results() {
    let fake = Fake::default();
    fake.with(|s| {
        s.states = VecDeque::from([
            json!({"title": "Amazon.ca Something Went Wrong"}),
            json!({"title": ""}),
            json!({"title": "Amazon.ca Something Went Wrong"}),
            json!({"title": "Amazon.ca", "continueButton": true}),
            json!({"title": "Amazon.ca : aeropress", "search": true}),
        ]);
        s.extract = json!({
            "total": "1-48 of over 1,000 results for",
            "results": [
                {"asin": "B0047BIWSK", "title": "AeroPress Original", "price": "$49.95",
                 "rating": "4.7 out of 5 stars", "reviewCount": "(30.1K)", "sponsored": false},
                {"asin": "B0SPONSOR1", "title": "Ad", "sponsored": true}
            ]
        })
        .to_string();
    });
    let amazon = service(&fake);
    let out = amazon
        .search("aeropress", Sort::Relevance, options())
        .await
        .unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.results[0].asin, "B0047BIWSK");
    assert_eq!(out.results[0].rating, Some(4.7));
    assert_eq!(out.sponsored_hidden, 1);
    assert_eq!(out.currency, "CAD");
    assert_eq!(
        fake.log(),
        [
            "goto https://www.amazon.ca/s?k=aeropress",
            "state",
            "goto https://www.amazon.ca/",
            "state",
            "wait",
            "goto https://www.amazon.ca/s?k=aeropress",
            "state",
            "goto https://www.amazon.ca/",
            "state",
            "continue",
            "wait",
            "goto https://www.amazon.ca/s?k=aeropress",
            "state",
            "extract",
        ]
    );

    // The warm browser serves the next call directly.
    fake.with(|s| {
        s.log.clear();
        s.states = VecDeque::from([json!({"search": true})]);
    });
    amazon
        .search("kettle", Sort::PriceLow, options())
        .await
        .unwrap();
    assert_eq!(
        fake.log(),
        [
            "goto https://www.amazon.ca/s?k=kettle&s=price-asc-rank",
            "state",
            "extract"
        ]
    );
    assert_eq!(fake.with(|s| s.launches), 1);
}

#[tokio::test(start_paused = true)]
async fn persistent_bot_check_gives_up_after_bounded_warm_ups() {
    let fake = Fake::default();
    let amazon = service(&fake);
    let error = amazon
        .search("x", Sort::Relevance, options())
        .await
        .unwrap_err();
    assert!(matches!(error, AmazonError::Blocked), "{error:?}");
    let searches = fake
        .log()
        .iter()
        .filter(|l| l.starts_with("goto https://www.amazon.ca/s?"))
        .count();
    assert_eq!(searches, 4);

    // Amazon flagged that session: the next call starts a fresh browser.
    fake.with(|s| s.states = VecDeque::from([json!({"search": true})]));
    fake.with(|s| s.extract = json!({"results": []}).to_string());
    amazon
        .search("x", Sort::Relevance, options())
        .await
        .unwrap();
    assert_eq!(fake.with(|s| s.launches), 2);
}

#[tokio::test(start_paused = true)]
async fn call_cancelled_mid_request_replaces_the_browser() {
    let fake = Fake::default();
    fake.with(|s| s.hang_navigation = true);
    let amazon = service(&fake);
    let cancelled = tokio::time::timeout(
        Duration::from_secs(1),
        amazon.search("x", Sort::Relevance, options()),
    )
    .await;
    assert!(cancelled.is_err());
    fake.with(|s| {
        s.states = VecDeque::from([json!({"search": true})]);
        s.extract = json!({"results": []}).to_string();
    });
    amazon
        .search("x", Sort::Relevance, options())
        .await
        .unwrap();
    assert_eq!(fake.with(|s| s.launches), 2);
}

#[tokio::test(start_paused = true)]
async fn image_captcha_stops_without_retrying() {
    let fake = Fake::default();
    fake.with(|s| {
        s.states = VecDeque::from([json!({"captcha": true, "continueButton": true})]);
    });
    let error = service(&fake)
        .search("x", Sort::Relevance, options())
        .await
        .unwrap_err();
    assert!(matches!(error, AmazonError::Captcha), "{error:?}");
    assert_eq!(fake.log().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn missing_product_is_not_found() {
    let fake = Fake::default();
    fake.with(|s| {
        s.states = VecDeque::from([json!({"title": "Page Not Found", "notFound": true})]);
    });
    let asin = Asin::parse("B0CFPJYX7P").unwrap();
    let error = service(&fake).product(&asin, Store::Ca).await.unwrap_err();
    assert!(matches!(error, AmazonError::NotFound), "{error:?}");
    assert_eq!(fake.log()[0], "goto https://www.amazon.ca/dp/B0CFPJYX7P");
}

#[tokio::test(start_paused = true)]
async fn failed_browser_is_replaced_on_the_next_call() {
    let fake = Fake::default();
    fake.with(|s| s.fail_navigation = true);
    let amazon = service(&fake);
    let error = amazon
        .search("x", Sort::Relevance, options())
        .await
        .unwrap_err();
    assert!(matches!(error, AmazonError::Browser(_)), "{error:?}");
    fake.with(|s| {
        s.states = VecDeque::from([json!({"product": true})]);
        s.extract = json!({"title": "AeroPress", "brand": "Visit the AeroPress Store",
            "price": "$49.95", "reviewCount": "(30,112)"})
        .to_string();
    });
    let product = amazon
        .product(&Asin::parse("B0047BIWSK").unwrap(), Store::Com)
        .await
        .unwrap();
    assert_eq!(product.brand.as_deref(), Some("AeroPress"));
    assert_eq!(product.review_count, Some(30_112));
    assert_eq!(product.url, "https://www.amazon.com/dp/B0047BIWSK");
    assert_eq!(fake.with(|s| s.launches), 2);
}
