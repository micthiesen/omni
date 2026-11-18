//! The `obscura` retriever: the page as a real browser renders it.
//!
//! It is one more candidate in the parallel set, for pages whose article
//! only appears after their scripts run (the plain fetch gets a preview or
//! an empty shell). The rendered HTML goes through the same parsers as a
//! fetched page and is rated with the other candidates.
//!
//! Each render starts its own Obscura process and ends it afterwards, so no
//! cookies or page state carry from one article to the next. The browser
//! only navigates to the submitted URL and reads the DOM: nothing is
//! clicked, typed or submitted, and Obscura itself refuses loopback,
//! private and link-local addresses for the page and everything it loads.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::obscura::{ObscuraBrowser, ObscuraLimits};
use omni_http::public::assert_public_http_url_syntax;
use serde::Deserialize;

use super::local::{blocking, postlight_article, readability_article};
use super::{ArticleRetriever, RetrievedPage, RetrieverContext};
use crate::error::PressPodsError;
use crate::types::Article;

const OPERATION: &str = "render article with Obscura";
/// The image installs Obscura on `PATH` (Dockerfile `obscura` stage).
pub const OBSCURA_PROGRAM: &str = "obscura";

/// Whole-render bound (start, navigation, settling, reading the DOM). Under
/// [`super::RETRIEVER_TIMEOUT`], so a slow page fails with this retriever's
/// own message and never holds the job past the shared retrieval budget.
pub const RENDER_TIMEOUT: Duration = Duration::from_secs(55);
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(30);
/// A page counts as settled once its text length has not changed for this
/// long. Measured on 2026-11-16, nytimes.com swapped its preview for the
/// article in steps up to 2.8 seconds apart, over 12 seconds after load.
const SETTLE_QUIET_MS: u32 = 4_000;
/// The longest the settle script waits for a page that never goes quiet.
const SETTLE_MAX_MS: u32 = 20_000;
/// How long a bot challenge may stay up before the wait is abandoned. In
/// testing on 2026-11-16 no Cloudflare or DataDome challenge cleared in
/// Obscura, even after 20 seconds.
const CHALLENGE_MAX_MS: u32 = 8_000;
/// Bound on the settle call; a busy page delays the script's own timers.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(24);
/// Largest rendered document accepted, in UTF-16 units of `outerHTML`.
pub const MAX_HTML_UNITS: usize = 5_000_000;
/// The document arrives as a JSON string inside a JSON-RPC text block, so
/// markup-heavy HTML is escaped twice.
const MAX_LINE_BYTES: u64 = 32 * 1024 * 1024;
/// Below this much paragraph text, a page showing a bot challenge is the
/// challenge rather than an article that happens to embed one.
const CHALLENGE_TEXT_UNITS: usize = 500;

/// What the browser read from the page.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedPage {
    /// Where the page ended up after its redirects.
    pub href: String,
    #[serde(default)]
    pub content_type: String,
    /// `outerHTML` length in UTF-16 units.
    pub chars: usize,
    /// Absent when the document is over [`MAX_HTML_UNITS`].
    #[serde(default)]
    pub html: Option<String>,
    /// The page is still a DataDome or Cloudflare bot challenge.
    #[serde(default)]
    pub challenge: bool,
    /// Text inside `<p>` elements, in UTF-16 units.
    #[serde(default)]
    pub paragraph_chars: usize,
}

/// Renders a public page in a browser.
pub trait PageRenderer: Send + Sync {
    fn render<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<RenderedPage, PressPodsError>>;
}

/// Waits until the document has loaded, shows no bot challenge and its
/// text has stopped changing (pages that swap a preview for the article do
/// so after load), then reports the DOM. Read-only.
fn settle_script() -> String {
    format!(
        r#"(async () => {{
  const size = () => (document.body ? document.body.textContent.length : 0);
  const challenged = () =>
    document.title === 'Just a moment...' ||
    !!document.querySelector(
      '#challenge-error-text, #challenge-form, iframe[src*="captcha-delivery.com"]'
    );
  const started = Date.now();
  let last = size(), quietSince = Date.now();
  while (Date.now() - started < {SETTLE_MAX_MS}) {{
    await new Promise((r) => setTimeout(r, 500));
    const now = size();
    if (challenged()) {{
      if (Date.now() - started >= {CHALLENGE_MAX_MS}) break;
      quietSince = Date.now();
    }} else if (now !== last || document.readyState !== 'complete') {{
      last = now;
      quietSince = Date.now();
    }} else if (Date.now() - quietSince >= {SETTLE_QUIET_MS}) {{
      break;
    }}
  }}
  const html = document.documentElement ? document.documentElement.outerHTML : '';
  let paragraphChars = 0;
  for (const p of document.querySelectorAll('p')) paragraphChars += p.textContent.length;
  return JSON.stringify({{
    href: location.href,
    contentType: document.contentType || '',
    chars: html.length,
    html: html.length > {MAX_HTML_UNITS} ? null : html,
    challenge: challenged(),
    paragraphChars,
  }});
}})()"#
    )
}

/// Launches `program mcp --stealth` for each render.
pub struct ObscuraRenderer {
    program: String,
}

impl ObscuraRenderer {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl PageRenderer for ObscuraRenderer {
    fn render<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<RenderedPage, PressPodsError>> {
        Box::pin(async move {
            let limits = ObscuraLimits {
                max_line_bytes: MAX_LINE_BYTES,
                navigate_timeout: NAVIGATE_TIMEOUT,
            };
            let failed = |e: omni_core::obscura::ObscuraError| {
                PressPodsError::failed(OPERATION, e.to_string())
            };
            // Dropped at the end of the render or with a timed-out one, which
            // kills the process.
            let mut browser = ObscuraBrowser::start(&self.program, limits)
                .await
                .map_err(failed)?;
            browser.navigate(url).await.map_err(failed)?;
            // A passed challenge reloads the page, which ends the script
            // that was watching it: watch the new document once more.
            let script = settle_script();
            let text = match browser.evaluate(&script, SETTLE_TIMEOUT).await {
                Ok(text) => text,
                Err(_) => browser
                    .evaluate(&script, SETTLE_TIMEOUT)
                    .await
                    .map_err(failed)?,
            };
            serde_json::from_str(&text).map_err(|e| {
                PressPodsError::invalid(OPERATION, format!("unreadable page report: {e}"))
            })
        })
    }
}

/// A rendered page's HTML, or why it cannot be an article candidate.
pub fn usable_html(page: RenderedPage) -> Result<String, PressPodsError> {
    // The browser follows redirects itself; a page that left the public web
    // (or http/https) is not read.
    assert_public_http_url_syntax(&page.href).map_err(|e| {
        PressPodsError::failed(
            OPERATION,
            format!("the page redirected off the public web: {e}"),
        )
    })?;
    let content_type = page.content_type.to_ascii_lowercase();
    if !(content_type.is_empty() || content_type.contains("html")) {
        return Err(PressPodsError::failed(
            OPERATION,
            format!("the page is {content_type}, not HTML"),
        ));
    }
    if page.challenge && page.paragraph_chars < CHALLENGE_TEXT_UNITS {
        return Err(PressPodsError::failed(
            OPERATION,
            "the site answered the browser with a bot challenge",
        ));
    }
    page.html.ok_or_else(|| {
        PressPodsError::failed(
            OPERATION,
            format!(
                "the rendered page is too large ({} characters, limit {MAX_HTML_UNITS})",
                page.chars
            ),
        )
    })
}

/// The page as Obscura renders it, parsed like a fetched page.
pub struct ObscuraRetriever {
    pub ctx: Arc<RetrieverContext>,
    pub renderer: Arc<dyn PageRenderer>,
}

impl ArticleRetriever for ObscuraRetriever {
    fn name(&self) -> &str {
        "obscura"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move { self.retrieve_page(url, user_agent).await?.article })
    }

    fn retrieve_page<'a>(
        &'a self,
        url: &'a str,
        _user_agent: &'a str,
    ) -> BoxFuture<'a, Result<RetrievedPage, PressPodsError>> {
        Box::pin(async move {
            assert_public_http_url_syntax(url).map_err(|e| PressPodsError::http(OPERATION, e))?;
            let page = tokio::time::timeout(RENDER_TIMEOUT, self.renderer.render(url))
                .await
                .map_err(|_| {
                    PressPodsError::failed(
                        OPERATION,
                        format!(
                            "no rendered page within {} seconds",
                            RENDER_TIMEOUT.as_secs()
                        ),
                    )
                })??;
            let href = page.href.clone();
            let html = usable_html(page)?;
            let url = url.to_owned();
            let tz = self.ctx.tz.clone();
            blocking(OPERATION, move || {
                // The strategy that wins most ratings on fetched pages, then
                // the plain Readability pass for pages its junk strip empties.
                let article = postlight_article(&html, &url, &tz)
                    .or_else(|_| readability_article(&html, &url, &tz));
                Ok(RetrievedPage {
                    html: Some(html),
                    url: href,
                    article,
                })
            })
            .await
        })
    }
}
