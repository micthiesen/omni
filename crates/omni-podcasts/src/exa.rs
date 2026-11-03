//! Exa podcast transcript search and retrieval (`docs/exa-podcasts.md`).
//!
//! Exa serves its podcast index through the ordinary `/search` and `/contents`
//! endpoints, restricted with `includeDomains: ["exa.ai/library/podcast"]`.
//! Every page Omni returns is an Exa podcast show or episode page, and text is
//! passed through verbatim: Omni never parses or invents speakers, timestamps
//! or episode identities beyond what the page URL carries.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use omni_ai::costs::{CostRecorder, NewCostEvent};
use omni_api::costs::{CostCategory, CostPriceStatus, CostUsage};
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, HttpError, Method, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

const LOG: &str = "ExaPodcasts";

pub const EXA_BASE_URL: &str = "https://api.exa.ai";
/// The `includeDomains` entry that limits a search to Exa's podcast index.
pub const PODCAST_INDEX: &str = "exa.ai/library/podcast";
const PAGE_PREFIX: &str = "https://exa.ai/library/podcast/";
const SEARCH_MAX_BYTES: usize = 2 * 1024 * 1024;
/// A full transcript at [`TRANSCRIPT_FETCH_MAX_CHARS`] JSON-escaped, plus links.
const CONTENTS_MAX_BYTES: usize = 12 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Characters of page text Omni retrieves per page; later text is reported as
/// `sourceTruncated`, never silently dropped.
pub const TRANSCRIPT_FETCH_MAX_CHARS: usize = 1_000_000;
pub const MAX_PAGE_LINKS: usize = 50;
pub const MAX_HIGHLIGHTS: usize = 3;
/// Exa's documented `numResults` ceiling is 100; Omni's tools ask for far fewer.
pub const MAX_SEARCH_RESULTS: usize = 10;
const CONCURRENT_REQUESTS: usize = 3;
/// Pages stay cached so paging through one transcript costs one Exa read.
const PAGE_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const PAGE_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// An Exa podcast page identity, taken only from its URL path.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PodcastPage {
    Show { show_id: String },
    Episode { show_id: String, episode_id: String },
}

impl PodcastPage {
    pub fn kind(&self) -> PageKind {
        match self {
            PodcastPage::Show { .. } => PageKind::Show,
            PodcastPage::Episode { .. } => PageKind::Episode,
        }
    }

    pub fn show_id(&self) -> &str {
        match self {
            PodcastPage::Show { show_id } | PodcastPage::Episode { show_id, .. } => show_id,
        }
    }

    pub fn episode_id(&self) -> Option<&str> {
        match self {
            PodcastPage::Show { .. } => None,
            PodcastPage::Episode { episode_id, .. } => Some(episode_id),
        }
    }

    pub fn canonical_url(&self) -> String {
        match self {
            PodcastPage::Show { show_id } => format!("{PAGE_PREFIX}{show_id}"),
            PodcastPage::Episode {
                show_id,
                episode_id,
            } => format!("{PAGE_PREFIX}{show_id}/episode/{episode_id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    Show,
    Episode,
    /// Under the podcast index but neither a show nor an episode page.
    Other,
}

fn valid_id(segment: &str) -> bool {
    (1..=64).contains(&segment.len())
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `https://exa.ai/library/podcast/<show>` or `.../<show>/episode/<episode>`;
/// a query, fragment or trailing slash is ignored. Anything else is `None`.
pub fn parse_podcast_url(raw: &str) -> Option<PodcastPage> {
    let url = Url::parse(raw.trim()).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("exa.ai")
        || url.port().is_some()
        || !url.username().is_empty()
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["library", "podcast", show] if valid_id(show) => Some(PodcastPage::Show {
            show_id: (*show).to_owned(),
        }),
        ["library", "podcast", show, "episode", episode] if valid_id(show) && valid_id(episode) => {
            Some(PodcastPage::Episode {
                show_id: (*show).to_owned(),
                episode_id: (*episode).to_owned(),
            })
        }
        _ => None,
    }
}

fn in_podcast_index(raw: &str) -> bool {
    Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("exa.ai")
            && url.path().starts_with("/library/podcast/")
    })
}

/// `YYYY-MM-DD` (midnight UTC) or an RFC 3339 instant, as the UTC instant Exa
/// expects for `startPublishedDate`/`endPublishedDate`.
pub fn normalize_date(value: &str) -> Option<String> {
    let value = value.trim();
    if let Ok(ts) = value.parse::<jiff::Timestamp>() {
        return Some(ts.to_string());
    }
    let date = value.parse::<jiff::civil::Date>().ok()?;
    let ts = date.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(ts.to_string())
}

/// The first `max` characters of `text` and whether anything was cut.
fn clip(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (text[..at].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ExaError {
    #[error("Exa request failed: HTTP {status}{}", tag.as_deref().map(|t| format!(" ({t})")).unwrap_or_default())]
    Status { status: u16, tag: Option<String> },
    #[error("Exa request failed: {0}")]
    Transport(String),
    /// The serde message is not kept: it can quote Exa's payload.
    #[error("Exa returned an invalid response")]
    Decode,
}

impl ExaError {
    pub fn is_transient(&self) -> bool {
        match self {
            ExaError::Status { status, .. } => *status == 429 || *status >= 500,
            ExaError::Transport(_) => true,
            ExaError::Decode => false,
        }
    }
}

impl From<HttpError> for ExaError {
    fn from(e: HttpError) -> Self {
        match e {
            // The body is Exa's; keep only the status.
            HttpError::Status { status, .. } => ExaError::Status { status, tag: None },
            other => ExaError::Transport(other.to_string()),
        }
    }
}

/// An Exa-supplied identifier (request id, error tag, content source) passed
/// to the caller only when it is short and plain, since it is untrusted text.
fn exa_token(value: Option<String>) -> Option<String> {
    value.filter(|v| {
        (1..=128).contains(&v.len())
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    })
}

// Exa wire format. Every field is optional or nullable, so a response with
// fewer fields still decodes; a result without a URL is skipped.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireResponse {
    request_id: Option<String>,
    results: Option<Vec<WireResult>>,
    statuses: Option<Vec<WireStatus>>,
    cost_dollars: Option<WireCost>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireResult {
    id: Option<String>,
    url: Option<String>,
    title: Option<String>,
    published_date: Option<String>,
    author: Option<String>,
    text: Option<String>,
    highlights: Option<Vec<String>>,
    extras: Option<WireExtras>,
}

#[derive(Deserialize)]
struct WireExtras {
    links: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStatus {
    id: String,
    status: String,
    source: Option<String>,
    error: Option<WireStatusError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStatusError {
    tag: Option<String>,
    http_status_code: Option<u16>,
}

#[derive(Deserialize)]
struct WireCost {
    total: Option<f64>,
}

#[derive(Deserialize)]
struct WireError {
    tag: Option<String>,
}

/// One search request.
#[derive(Clone, Debug)]
pub struct SearchParams {
    pub query: String,
    pub limit: usize,
    /// Normalized by [`normalize_date`].
    pub published_after: Option<String>,
    pub published_before: Option<String>,
    pub highlight_chars: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub url: String,
    pub kind: PageKind,
    pub show_id: Option<String>,
    pub episode_id: Option<String>,
    pub exa_id: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    /// Exa's publication date for the page, unchanged.
    pub published_date: Option<String>,
    pub highlights: Vec<String>,
    pub highlights_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    pub request_id: Option<String>,
    pub results: Vec<SearchHit>,
    /// Results outside the podcast index, which Omni drops.
    pub dropped_results: usize,
    pub cost_dollars: Option<f64>,
}

/// A retrieved podcast page, cached for paging.
#[derive(Clone, Debug)]
pub struct PageText {
    pub exa_id: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub published_date: Option<String>,
    /// Exa's `statuses[].source`: `cached` or `crawled`.
    pub content_source: Option<String>,
    pub text: String,
    pub char_count: usize,
    /// The page has more text than [`TRANSCRIPT_FETCH_MAX_CHARS`].
    pub source_truncated: bool,
    pub links: Vec<String>,
    pub links_truncated: bool,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// Exa reported an error for the page.
    ExaStatusError,
    /// Exa answered without the requested page.
    NotReturned,
    /// Exa returned the page with no text.
    EmptyText,
}

#[derive(Clone, Debug)]
pub struct Unavailable {
    pub reason: UnavailableReason,
    pub exa_error_tag: Option<String>,
    pub exa_http_status: Option<u16>,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum PageFetch {
    Available(Arc<PageText>),
    Unavailable(Unavailable),
}

#[derive(Clone, Debug)]
enum Transport {
    Public(PublicHttpClient),
    Plain(HttpClient),
}

impl Transport {
    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        match self {
            Transport::Public(client) => client.request(method, url),
            Transport::Plain(client) => client.request(method, url),
        }
    }
}

/// The Exa client for podcast pages only.
pub struct ExaPodcasts {
    transport: Transport,
    api_key: String,
    costs: CostRecorder,
    permits: Semaphore,
    pages: Cache<String, Arc<PageText>>,
}

impl ExaPodcasts {
    /// Production: the SSRF-guarded public client.
    pub fn new(http: PublicHttpClient, api_key: String, costs: CostRecorder) -> Self {
        Self::build(Transport::Public(http), api_key, costs)
    }

    /// A plain client (tests rewrite Exa's origin to a local mock server).
    pub fn with_http(http: HttpClient, api_key: String, costs: CostRecorder) -> Self {
        Self::build(Transport::Plain(http), api_key, costs)
    }

    fn build(transport: Transport, api_key: String, costs: CostRecorder) -> Self {
        Self {
            transport,
            api_key,
            costs,
            permits: Semaphore::new(CONCURRENT_REQUESTS),
            pages: Cache::builder()
                .time_to_live(PAGE_CACHE_TTL)
                .weigher(|_, page: &Arc<PageText>| {
                    u32::try_from(page.text.len()).unwrap_or(u32::MAX)
                })
                .max_capacity(PAGE_CACHE_MAX_BYTES)
                .build(),
        }
    }

    /// `None` when `EXA_API_KEY` is unset.
    pub fn from_config(
        config: &omni_config::Config,
        http: PublicHttpClient,
        costs: CostRecorder,
    ) -> Option<Self> {
        let key = config
            .exa_api_key
            .clone()
            .filter(|k| !k.trim().is_empty())?;
        Some(Self::new(http, key, costs))
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
        max_bytes: usize,
    ) -> Result<WireResponse, ExaError> {
        let url = Url::parse(&format!("{EXA_BASE_URL}{path}"))
            .map_err(|e| ExaError::Transport(e.to_string()))?;
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| ExaError::Transport(e.to_string()))?;
        let response = self
            .transport
            .request(Method::POST, url)
            .header("x-api-key", self.api_key.as_str())
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send_bounded(max_bytes)
            .await?;
        let status = response.status.as_u16();
        if !response.status.is_success() {
            let tag = exa_token(
                serde_json::from_slice::<WireError>(&response.body)
                    .ok()
                    .and_then(|e| e.tag),
            );
            return Err(ExaError::Status { status, tag });
        }
        serde_json::from_slice(&response.body).map_err(|_| ExaError::Decode)
    }

    async fn record_cost(&self, operation: &str, category: CostCategory, cost: Option<f64>) {
        let cents = cost
            .filter(|c| c.is_finite() && *c >= 0.0)
            .map(|c| c * 100.0);
        self.costs
            .record(NewCostEvent {
                category,
                feature: "podcast-transcripts".to_owned(),
                operation: operation.to_owned(),
                service: "exa".to_owned(),
                model: None,
                cost_cents: cents,
                price_status: if cents.is_some() {
                    CostPriceStatus::Priced
                } else {
                    CostPriceStatus::Unknown
                },
                usage: CostUsage {
                    requests: Some(1.0),
                    ..CostUsage::default()
                },
                event_id: None,
                incurred_at: None,
                run_id: None,
            })
            .await;
    }

    /// One search over the podcast index; results outside it are dropped.
    pub async fn search(&self, params: SearchParams) -> Result<SearchResults, ExaError> {
        let mut body = json!({
            "query": params.query,
            "type": "auto",
            "numResults": params.limit.clamp(1, MAX_SEARCH_RESULTS),
            "includeDomains": [PODCAST_INDEX],
            "contents": { "highlights": { "maxCharacters": params.highlight_chars } },
        });
        if let Some(after) = &params.published_after {
            body["startPublishedDate"] = json!(after);
        }
        if let Some(before) = &params.published_before {
            body["endPublishedDate"] = json!(before);
        }
        let response = self.post("/search", body, SEARCH_MAX_BYTES).await?;
        let cost = response.cost_dollars.as_ref().and_then(|c| c.total);
        self.record_cost("search", CostCategory::Search, cost).await;

        let mut dropped = 0;
        let mut results = Vec::new();
        for result in response.results.unwrap_or_default() {
            let Some(url) = result.url.filter(|u| in_podcast_index(u)) else {
                dropped += 1;
                continue;
            };
            if results.len() >= params.limit {
                break;
            }
            let page = parse_podcast_url(&url);
            let mut truncated = false;
            let highlights: Vec<String> = result
                .highlights
                .unwrap_or_default()
                .iter()
                .enumerate()
                .filter_map(|(i, h)| {
                    if i >= MAX_HIGHLIGHTS {
                        truncated = true;
                        return None;
                    }
                    let (text, cut) = clip(h, params.highlight_chars);
                    truncated |= cut;
                    Some(text)
                })
                .collect();
            results.push(SearchHit {
                kind: page.as_ref().map_or(PageKind::Other, PodcastPage::kind),
                show_id: page.as_ref().map(|p| p.show_id().to_owned()),
                episode_id: page
                    .as_ref()
                    .and_then(|p| p.episode_id().map(str::to_owned)),
                url,
                exa_id: result.id,
                title: result.title,
                author: result.author,
                published_date: result.published_date,
                highlights,
                highlights_truncated: truncated,
            });
        }
        if dropped > 0 {
            tracing::warn!(target: LOG, dropped, "Exa returned results outside the podcast index");
        }
        Ok(SearchResults {
            request_id: exa_token(response.request_id),
            results,
            dropped_results: dropped,
            cost_dollars: cost,
        })
    }

    /// The page's text and links, from the cache or one `/contents` read
    /// shared by concurrent callers. Unavailable pages are not cached.
    pub async fn page(&self, page: &PodcastPage) -> Result<PageFetch, ExaError> {
        let url = page.canonical_url();
        match self
            .pages
            .try_get_with(url.clone(), self.fetch_page(page, url))
            .await
        {
            Ok(text) => Ok(PageFetch::Available(text)),
            Err(miss) => match &*miss {
                PageMiss::Unavailable(u) => Ok(PageFetch::Unavailable(u.clone())),
                PageMiss::Failed(e) => Err(e.clone()),
            },
        }
    }

    async fn fetch_page(&self, page: &PodcastPage, url: String) -> Result<Arc<PageText>, PageMiss> {
        let body = json!({
            "urls": [url],
            "text": { "maxCharacters": TRANSCRIPT_FETCH_MAX_CHARS + 1 },
            "extras": { "links": MAX_PAGE_LINKS + 1 },
        });
        let response = self
            .post("/contents", body, CONTENTS_MAX_BYTES)
            .await
            .map_err(PageMiss::Failed)?;
        let cost = response.cost_dollars.as_ref().and_then(|c| c.total);
        self.record_cost("contents", CostCategory::Retrieval, cost)
            .await;

        let request_id = exa_token(response.request_id);
        let status = response
            .statuses
            .unwrap_or_default()
            .into_iter()
            .find(|s| parse_podcast_url(&s.id).as_ref() == Some(page) || s.id == url);
        let unavailable = |reason, status: Option<&WireStatus>| {
            let error = status.and_then(|s| s.error.as_ref());
            PageMiss::Unavailable(Unavailable {
                reason,
                exa_error_tag: exa_token(error.and_then(|e| e.tag.clone())),
                exa_http_status: error.and_then(|e| e.http_status_code),
                request_id: request_id.clone(),
            })
        };
        if status.as_ref().is_some_and(|s| s.status != "success") {
            return Err(unavailable(
                UnavailableReason::ExaStatusError,
                status.as_ref(),
            ));
        }
        let Some(result) = response
            .results
            .unwrap_or_default()
            .into_iter()
            .find(|r| r.url.as_deref().and_then(parse_podcast_url).as_ref() == Some(page))
        else {
            return Err(unavailable(UnavailableReason::NotReturned, status.as_ref()));
        };
        let raw_text = result.text.unwrap_or_default();
        if raw_text.trim().is_empty() {
            return Err(unavailable(UnavailableReason::EmptyText, status.as_ref()));
        }
        let (text, source_truncated) = clip(&raw_text, TRANSCRIPT_FETCH_MAX_CHARS);
        let mut links = result.extras.and_then(|e| e.links).unwrap_or_default();
        let links_truncated = links.len() > MAX_PAGE_LINKS;
        links.truncate(MAX_PAGE_LINKS);
        Ok(Arc::new(PageText {
            exa_id: result.id,
            title: result.title,
            author: result.author,
            published_date: result.published_date,
            content_source: exa_token(status.and_then(|s| s.source)),
            char_count: text.chars().count(),
            text,
            source_truncated,
            links,
            links_truncated,
            request_id,
        }))
    }
}

/// Why a page read produced nothing to cache.
enum PageMiss {
    Unavailable(Unavailable),
    Failed(ExaError),
}

/// `[offset, offset + max)` in characters, and the next offset when more remains.
pub fn page_slice(text: &str, offset: usize, max: usize) -> (String, Option<usize>) {
    let start = text
        .char_indices()
        .nth(offset)
        .map_or(text.len(), |(at, _)| at);
    let rest = &text[start..];
    let (slice, more) = clip(rest, max);
    (slice, more.then_some(offset + max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_show_and_episode_urls_only() {
        assert_eq!(
            parse_podcast_url("https://exa.ai/library/podcast/tpr3b0f359y/episode/s20kmckm3v9"),
            Some(PodcastPage::Episode {
                show_id: "tpr3b0f359y".to_owned(),
                episode_id: "s20kmckm3v9".to_owned(),
            })
        );
        assert_eq!(
            parse_podcast_url("https://exa.ai/library/podcast/tpr3b0f359y/?ref=x#t"),
            Some(PodcastPage::Show {
                show_id: "tpr3b0f359y".to_owned()
            })
        );
        for bad in [
            "http://exa.ai/library/podcast/abc",
            "https://evil.example/library/podcast/abc",
            "https://exa.ai.evil.example/library/podcast/abc",
            "https://user@exa.ai/library/podcast/abc",
            "https://exa.ai:8443/library/podcast/abc",
            "https://exa.ai/library/podcast",
            "https://exa.ai/library/podcast/abc/episode",
            "https://exa.ai/library/podcast/a%2Fb",
            "https://exa.ai/library/paper/abc",
            "not a url",
        ] {
            assert_eq!(parse_podcast_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn canonical_url_round_trips() {
        let page = PodcastPage::Episode {
            show_id: "s".to_owned(),
            episode_id: "e".to_owned(),
        };
        assert_eq!(parse_podcast_url(&page.canonical_url()), Some(page));
    }

    #[test]
    fn normalizes_dates_to_utc_instants() {
        assert_eq!(
            normalize_date("2026-10-01").as_deref(),
            Some("2026-10-01T00:00:00Z")
        );
        assert_eq!(
            normalize_date("2026-10-01T12:30:00-07:00").as_deref(),
            Some("2026-10-01T19:30:00Z")
        );
        assert_eq!(normalize_date("last week"), None);
    }

    #[test]
    fn slices_by_character_and_reports_the_next_offset() {
        let text = "ab\u{e9}cd";
        assert_eq!(page_slice(text, 0, 3), ("ab\u{e9}".to_owned(), Some(3)));
        assert_eq!(page_slice(text, 3, 3), ("cd".to_owned(), None));
        assert_eq!(page_slice(text, 0, 5), (text.to_owned(), None));
        assert_eq!(page_slice(text, 9, 3), (String::new(), None));
    }
}
