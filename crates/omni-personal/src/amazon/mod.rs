//! Amazon search and product pages read through a headless Obscura browser.
//!
//! Amazon answers a fresh browser with a bot check (an AWS WAF challenge, then
//! a "Continue shopping" interstitial). Loading the storefront's home page,
//! pressing continue and letting the page run for a few seconds clears it, and
//! the cookies then carry later requests. One browser process is kept warm and
//! shared; calls are serialized because it has one page. The process is
//! replaced after a browser failure, a timed-out call, or [`MAX_LOADS`] pages.
//! Nothing is bought or signed in to: prices are the logged-out ones.

pub mod browser;
pub mod page;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use self::browser::{Browser, Launcher};
use self::page::{
    Asin, CONTINUE_SCRIPT, PRODUCT_SCRIPT, PageKind, PageState, Product, RawProduct, RawSearch,
    SEARCH_SCRIPT, STATE_SCRIPT, SearchOptions, SearchResults, Sort, Verdict,
};

const LOG: &str = "Amazon";
/// Warm-ups per load; two clear both bot checks in practice.
const MAX_WARMUPS: usize = 3;
/// Pages served by one browser process before it is replaced.
const MAX_LOADS: u32 = 200;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-call bound, including warm-ups.
const CALL_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Debug, thiserror::Error)]
pub enum AmazonError {
    #[error("the Amazon browser is unavailable: {0}")]
    Unavailable(String),
    #[error("browser error: {0}")]
    Browser(String),
    #[error("Amazon kept showing its bot check; try again in a few minutes")]
    Blocked,
    #[error("Amazon is asking for an image CAPTCHA, which this tool cannot solve")]
    Captcha,
    #[error("no Amazon product page exists for that ASIN in this store")]
    NotFound,
    #[error("the Amazon request took longer than {0} seconds")]
    Timeout(u64),
    #[error("Amazon's page could not be read: {0}")]
    Extract(String),
}

impl AmazonError {
    /// Whether to start the next call with a fresh process and cookie jar:
    /// the browser may be mid-request, or Amazon has flagged its session.
    fn resets_browser(&self) -> bool {
        matches!(
            self,
            Self::Browser(_) | Self::Timeout(_) | Self::Captcha | Self::Blocked
        )
    }

    /// Outcomes about Amazon or the request rather than a fault here.
    fn expected(&self) -> bool {
        matches!(self, Self::NotFound | Self::Captcha | Self::Blocked)
    }
}

#[derive(Default)]
struct Session {
    browser: Option<Box<dyn Browser>>,
    loads: u32,
    /// Set while a call runs; still set at the next call when the previous
    /// one was cancelled mid-request, leaving the browser's stdio unknown.
    busy: bool,
}

/// Reads Amazon through one shared browser.
pub struct AmazonService {
    launcher: Arc<dyn Launcher>,
    session: Mutex<Session>,
    /// How long a warm-up lets Amazon's challenge script run.
    settle: Duration,
}

impl AmazonService {
    pub fn new(launcher: Arc<dyn Launcher>) -> Self {
        Self::with_settle(launcher, Duration::from_secs(8))
    }

    pub fn with_settle(launcher: Arc<dyn Launcher>, settle: Duration) -> Self {
        Self {
            launcher,
            session: Mutex::new(Session::default()),
            settle,
        }
    }

    pub async fn search(
        &self,
        query: &str,
        sort: Sort,
        options: SearchOptions,
    ) -> Result<SearchResults, AmazonError> {
        let url = page::search_url(options.store, query, options.page, sort);
        let raw: RawSearch = self
            .read(options.store, &url, PageKind::Search, SEARCH_SCRIPT)
            .await?;
        tracing::info!(target: LOG, store = ?options.store, results = raw.results.len(), "Amazon search");
        Ok(page::search_results(raw, url, &options))
    }

    pub async fn product(&self, asin: &Asin, store: page::Store) -> Result<Product, AmazonError> {
        let url = store.product_url(asin);
        let raw: RawProduct = self
            .read(store, &url, PageKind::Product, PRODUCT_SCRIPT)
            .await?;
        page::product(raw, asin, store)
            .ok_or_else(|| AmazonError::Extract("the product page has no title".into()))
    }

    async fn read<T: serde::de::DeserializeOwned>(
        &self,
        store: page::Store,
        url: &str,
        kind: PageKind,
        script: &str,
    ) -> Result<T, AmazonError> {
        let mut session = self.session.lock().await;
        if session.busy {
            session.browser = None;
        }
        session.busy = true;
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.load_and_extract(&mut session, store, url, kind, script),
        )
        .await
        .unwrap_or(Err(AmazonError::Timeout(CALL_TIMEOUT.as_secs())));
        session.busy = false;
        if let Err(error) = &result {
            if error.resets_browser() {
                session.browser = None;
            }
            if error.expected() {
                tracing::info!(target: LOG, "Amazon read: {error}");
            } else {
                tracing::warn!(target: LOG, "Amazon read failed: {error}");
            }
        }
        result
    }

    async fn load_and_extract<T: serde::de::DeserializeOwned>(
        &self,
        session: &mut Session,
        store: page::Store,
        url: &str,
        kind: PageKind,
        script: &str,
    ) -> Result<T, AmazonError> {
        if session.loads >= MAX_LOADS {
            session.browser = None;
        }
        if session.browser.is_none() {
            session.browser = Some(self.launcher.launch().await?);
            session.loads = 0;
        }
        let Some(browser) = session.browser.as_mut() else {
            return Err(AmazonError::Unavailable("the browser did not start".into()));
        };
        for attempt in 0..=MAX_WARMUPS {
            session.loads += 1;
            browser.navigate(url).await?;
            let page = state(browser.as_mut()).await?;
            let verdict = page.verdict(kind);
            tracing::debug!(target: LOG, attempt, ?verdict, title = %page.title, "Amazon page");
            match verdict {
                Verdict::Ready => {
                    let text = browser.evaluate(script, SCRIPT_TIMEOUT).await?;
                    return serde_json::from_str(&text)
                        .map_err(|e| AmazonError::Extract(e.to_string()));
                }
                Verdict::NotFound => return Err(AmazonError::NotFound),
                Verdict::Captcha => return Err(AmazonError::Captcha),
                Verdict::Blocked if attempt == MAX_WARMUPS => break,
                Verdict::Blocked => self.warm_up(browser.as_mut(), store).await?,
            }
        }
        Err(AmazonError::Blocked)
    }

    /// Loads the home page, passes the interstitial, and lets the challenge run.
    async fn warm_up(
        &self,
        browser: &mut dyn Browser,
        store: page::Store,
    ) -> Result<(), AmazonError> {
        browser.navigate(&store.home_url()).await?;
        let home = state(browser).await?;
        if home.captcha {
            return Err(AmazonError::Captcha);
        }
        if home.continue_button {
            browser.evaluate(CONTINUE_SCRIPT, SCRIPT_TIMEOUT).await?;
        }
        let millis = self.settle.as_millis();
        let wait = format!("new Promise((r) => setTimeout(() => r(true), {millis}))");
        browser
            .evaluate(&wait, self.settle + SCRIPT_TIMEOUT)
            .await?;
        Ok(())
    }
}

async fn state(browser: &mut dyn Browser) -> Result<PageState, AmazonError> {
    let text = browser.evaluate(STATE_SCRIPT, SCRIPT_TIMEOUT).await?;
    serde_json::from_str(&text).map_err(|e| AmazonError::Extract(format!("page state: {e}")))
}
