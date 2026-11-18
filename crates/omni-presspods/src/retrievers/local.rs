//! The HTML-parsing retrievers.
//!
//! Each retriever keeps its persisted name and uses a deliberately different
//! strategy; the metadata model rates every result and the best one wins, so
//! diversity between them is what matters:
//!
//! - `readability`: Mozilla Readability (`dom_smoothie`, a faithful port),
//!   lead image from `og:image`.
//! - `postlight`: Mercury-style. Strips page chrome, ads and social widgets
//!   first (Mercury's generic cleaners), runs the Readability scorer on what is
//!   left, and takes Mercury's metadata order (meta tags, JSON-LD, then byline
//!   markup and `<time datetime>`).
//! - `extractus`: structured-data first. A JSON-LD `articleBody` (present on
//!   most news sites and immune to layout) wins; otherwise the densest
//!   `<article>`/`main`/content container by paragraph text.
//! - `fetch`: the whole page, cleaned.
//!
//! Crate choice: `dom_smoothie` is a maintained, pure-Rust port of Mozilla
//! Readability over `dom_query` (html5ever) that tracks upstream's scoring and
//! metadata. The older `readability` crate is an unmaintained partial port, and
//! `article_scraper` needs libxml2 plus per-site config files. There is no Rust
//! Mercury or article-extractor, so those two strategies are built here on
//! `scraper`; the Mercury-style junk strip never removes an element that holds
//! the article (see `html::strip_junk`).
//!
//! Parsing is CPU-bound, so it runs on the blocking pool.

use std::sync::Arc;

use futures::future::BoxFuture;
use scraper::{ElementRef, Html};

use super::html::{
    collapse, extract_domain, extract_title_from_html, page_meta, paragraph_chars, selector,
    strip_junk,
};
use super::{ArticleRetriever, RetrievedPage, RetrieverContext};
use crate::dates::parse_js_date;
use crate::error::PressPodsError;
use crate::formatting::clean_text;
use crate::public_http::{PRESS_PODS_HTML_MAX_BYTES, fetch_public_html};
use crate::types::Article;

pub(super) async fn blocking<T, F>(operation: &'static str, f: F) -> Result<T, PressPodsError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PressPodsError> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| PressPodsError::failed(operation, e.to_string()))?
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// The whole page, cleaned.
pub struct FetchRetriever(pub Arc<RetrieverContext>);

impl ArticleRetriever for FetchRetriever {
    fn name(&self) -> &str {
        "fetch"
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
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<RetrievedPage, PressPodsError>> {
        Box::pin(async move {
            let html = fetch_public_html(
                &self.0.public_http,
                url,
                user_agent,
                PRESS_PODS_HTML_MAX_BYTES,
            )
            .await?;
            let url = url.to_owned();
            blocking("clean fetched article", move || {
                let article = clean_text(&html).map(|text| Article {
                    title: extract_title_from_html(&html),
                    text,
                    author: None,
                    domain: extract_domain(&url),
                    published_at: None,
                    lead_image_url: None,
                    url: url.clone(),
                });
                Ok(RetrievedPage {
                    html: Some(html),
                    url,
                    article,
                })
            })
            .await
        })
    }
}

/// Readability parse of `html`: `(title, byline, published, content html)`.
fn readability_parse(
    html: &str,
    url: &str,
    operation: &'static str,
) -> Result<(String, Option<String>, Option<String>, String), PressPodsError> {
    let mut readability = dom_smoothie::Readability::new(html, Some(url), None)
        .map_err(|e| PressPodsError::failed(operation, e.to_string()))?;
    let article = readability
        .parse()
        .map_err(|e| PressPodsError::failed(operation, e.to_string()))?;
    let content = article.content.to_string();
    if content.trim().is_empty() {
        return Err(PressPodsError::failed(
            operation,
            "Readability could not parse the article",
        ));
    }
    Ok((
        article.title,
        article.byline,
        article.published_time,
        content,
    ))
}

/// Mozilla Readability.
pub struct ReadabilityRetriever(pub Arc<RetrieverContext>);

impl ArticleRetriever for ReadabilityRetriever {
    fn name(&self) -> &str {
        "readability"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move {
            let html = fetch_public_html(
                &self.0.public_http,
                url,
                user_agent,
                PRESS_PODS_HTML_MAX_BYTES,
            )
            .await?;
            let url = url.to_owned();
            let tz = self.0.tz.clone();
            blocking("parse article with Readability", move || {
                readability_article(&html, &url, &tz)
            })
            .await
        })
    }
}

/// The `readability` strategy over already-fetched HTML.
pub(super) fn readability_article(
    html: &str,
    url: &str,
    tz: &jiff::tz::TimeZone,
) -> Result<Article, PressPodsError> {
    const OPERATION: &str = "parse article with Readability";
    let lead_image = {
        let document = Html::parse_document(html);
        super::html::meta_content(&document, &[r#"meta[property="og:image"]"#])
    };
    let (title, byline, published, content) = readability_parse(html, url, OPERATION)?;
    Ok(Article {
        title: non_empty(Some(title)),
        text: clean_text(&content)?,
        author: non_empty(byline),
        domain: extract_domain(url),
        published_at: published.and_then(|p| parse_js_date(&p, tz)),
        lead_image_url: lead_image,
        url: url.to_owned(),
    })
}

/// Mercury-style cleanup, scoring and metadata.
pub struct PostlightRetriever(pub Arc<RetrieverContext>);

impl ArticleRetriever for PostlightRetriever {
    fn name(&self) -> &str {
        "postlight"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move {
            let html = fetch_public_html(
                &self.0.public_http,
                url,
                user_agent,
                PRESS_PODS_HTML_MAX_BYTES,
            )
            .await?;
            let url = url.to_owned();
            let tz = self.0.tz.clone();
            blocking("parse article with Postlight", move || {
                postlight_article(&html, &url, &tz)
            })
            .await
        })
    }
}

/// The `postlight` strategy over already-fetched HTML.
pub(super) fn postlight_article(
    html: &str,
    url: &str,
    tz: &jiff::tz::TimeZone,
) -> Result<Article, PressPodsError> {
    const OPERATION: &str = "parse article with Postlight";
    let meta = page_meta(&Html::parse_document(html));
    let cleaned = strip_junk(html);
    let (title, byline, published, content) = readability_parse(&cleaned, url, OPERATION)?;
    tracing::debug!(target: "PressPods.retrievers.postlight", chars = content.len(), "Parsed article with postlight");
    Ok(Article {
        title: non_empty(meta.title).or_else(|| non_empty(Some(title))),
        text: clean_text(&content)?,
        author: non_empty(meta.author).or_else(|| non_empty(byline)),
        domain: extract_domain(url),
        published_at: meta
            .published
            .or(published)
            .and_then(|p| parse_js_date(&p, tz)),
        lead_image_url: non_empty(meta.image),
        url: url.to_owned(),
    })
}

/// Containers that usually hold the article body, best first.
const CONTENT_CANDIDATES: &[&str] = &[
    "[itemprop=articleBody]",
    "article",
    ".article-body",
    ".article-content",
    ".post-content",
    ".entry-content",
    ".story-body",
    "main",
    "[role=main]",
    "#content",
];

/// Body elements kept when rebuilding the article from its container.
const CONTENT_BLOCKS: &str = "h1, h2, h3, h4, p, blockquote, ul, ol, pre";

/// Rebuilds clean HTML from the densest content container.
fn densest_container_html(document: &Html) -> Option<String> {
    let best: Option<ElementRef<'_>> = CONTENT_CANDIDATES
        .iter()
        .filter_map(|css| selector(css))
        .flat_map(|sel| document.select(&sel).collect::<Vec<_>>())
        .max_by_key(|el| paragraph_chars(*el));
    let container = match best.filter(|el| paragraph_chars(*el) >= 200) {
        Some(container) => container,
        None => {
            let body = selector("body")?;
            document.select(&body).next()?
        }
    };
    let blocks = selector(CONTENT_BLOCKS)?;
    let mut out = String::new();
    for block in container.select(&blocks) {
        // Nested blocks (a <p> inside a kept <blockquote>) render with their parent.
        let nested = block
            .ancestors()
            .filter_map(ElementRef::wrap)
            .take_while(|a| a.id() != container.id())
            .any(|a| matches!(a.value().name(), "blockquote" | "ul" | "ol" | "pre"));
        if nested {
            continue;
        }
        let text = collapse(&block.text().collect::<String>());
        if block.value().name() == "p" && text.chars().count() < 25 {
            continue;
        }
        out.push_str(&block.html());
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Structured data first, then content density.
pub struct ExtractusRetriever(pub Arc<RetrieverContext>);

impl ArticleRetriever for ExtractusRetriever {
    fn name(&self) -> &str {
        "extractus"
    }

    fn retrieve<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        Box::pin(async move {
            let html = fetch_public_html(
                &self.0.public_http,
                url,
                user_agent,
                PRESS_PODS_HTML_MAX_BYTES,
            )
            .await?;
            let url = url.to_owned();
            let tz = self.0.tz.clone();
            blocking("parse article with Extractus", move || {
                const OPERATION: &str = "parse article with Extractus";
                let document = Html::parse_document(&html);
                let meta = page_meta(&document);
                let content = match meta
                    .json_ld_body
                    .as_deref()
                    .filter(|b| b.chars().count() >= 200)
                {
                    Some(body) => body
                        .split('\n')
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(|p| format!("<p>{}</p>", crate::rss::escape_xml(p)))
                        .collect::<String>(),
                    None => densest_container_html(&document).ok_or_else(|| {
                        PressPodsError::failed(OPERATION, "Failed to extract article")
                    })?,
                };
                Ok(Article {
                    title: non_empty(meta.title),
                    text: clean_text(&content)?,
                    author: non_empty(meta.author),
                    domain: extract_domain(&url),
                    published_at: meta.published.and_then(|p| parse_js_date(&p, &tz)),
                    lead_image_url: non_empty(meta.image),
                    url,
                })
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<html><head><title>Page Title</title>
        <meta property="og:title" content="OG Title">
        <meta name="author" content="Jane Writer">
        <meta property="article:published_time" content="2026-07-14T14:04:31Z">
        <meta property="og:image" content="https://img.test/lead.jpg">
        </head><body><nav><a href="/">Home</a> <a href="/news">News</a></nav>
        <div class="share-tools">Share this on every network</div>
        <article><h1>OG Title</h1>
        <p>The first paragraph of the article explains the premise in enough words to count as content.</p>
        <p>The second paragraph continues the story with more detail about what happened and why it matters.</p>
        <blockquote><p>A quoted passage that the narration should keep.</p></blockquote>
        <p>The third paragraph wraps up the argument and leaves the reader with a final thought to ponder.</p>
        </article><footer>Copyright and links</footer></body></html>"#;

    #[test]
    fn densest_container_keeps_body_blocks() {
        let html = densest_container_html(&Html::parse_document(PAGE)).unwrap();
        assert!(html.contains("first paragraph"));
        assert!(html.contains("<blockquote>"));
        assert!(!html.contains("Share this"));
        let text = clean_text(&html).unwrap();
        assert!(text.contains("> A quoted passage"), "{text}");
    }

    #[test]
    fn readability_and_mercury_style_parsers_find_the_article() {
        let (_, _, _, content) = readability_parse(PAGE, "https://news.test/a", "test").unwrap();
        assert!(content.contains("second paragraph"));
        let cleaned = strip_junk(PAGE);
        let (_, _, _, content) =
            readability_parse(&cleaned, "https://news.test/a", "test").unwrap();
        assert!(content.contains("third paragraph"));
        assert!(!content.contains("Share this"));
        let meta = page_meta(&Html::parse_document(PAGE));
        assert_eq!(meta.title.as_deref(), Some("OG Title"));
        assert_eq!(meta.author.as_deref(), Some("Jane Writer"));
        assert_eq!(meta.image.as_deref(), Some("https://img.test/lead.jpg"));
    }
}
