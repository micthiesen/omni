//! Podcast Index `search/byperson`.

use std::time::Duration;

use futures::future::BoxFuture;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method, Url};
use serde::Deserialize;
use sha1::{Digest as _, Sha1};

use crate::pacing::RequestPacer;

const LOG: &str = "PodcastIndex";
const BASE_URL: &str = "https://api.podcastindex.org/api/1.0";
const DEFAULT_MAX_RESULTS: &str = "20";
const RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RETRIES: u32 = 2;
const RETRY_BASE: Duration = Duration::from_millis(200);
const AUTH_USER_AGENT: &str = "omni/1.0";

#[derive(Clone)]
pub struct PodcastIndexCredentials {
    pub key: String,
    pub secret: String,
}

/// `sha1(key + secret + authDateSeconds)` as lowercase hex.
pub fn podcast_index_auth_hash(key: &str, secret: &str, auth_date_seconds: &str) -> String {
    hex::encode(Sha1::digest(
        format!("{key}{secret}{auth_date_seconds}").as_bytes(),
    ))
}

/// The four auth headers, in order (`X-Auth-Key`, `X-Auth-Date`,
/// `Authorization`, `User-Agent`).
pub fn podcast_index_auth_headers(
    creds: &PodcastIndexCredentials,
    now_ms: i64,
) -> Vec<(&'static str, String)> {
    let auth_date = now_ms.div_euclid(1000).to_string();
    vec![
        ("X-Auth-Key", creds.key.clone()),
        ("X-Auth-Date", auth_date.clone()),
        (
            "Authorization",
            podcast_index_auth_hash(&creds.key, &creds.secret, &auth_date),
        ),
        ("User-Agent", AUTH_USER_AGENT.to_owned()),
    ]
}

/// One search result; every field optional and nullable (the API sends `null`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPodcastIndexEpisode {
    pub title: Option<String>,
    pub feed_title: Option<String>,
    pub feed_url: Option<String>,
    pub feed_itunes_id: Option<f64>,
    pub guid: Option<String>,
    pub enclosure_url: Option<String>,
    pub link: Option<String>,
    pub date_published: Option<f64>,
    pub duration: Option<f64>,
    pub description: Option<String>,
    pub image: Option<String>,
    pub feed_image: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SearchByPersonResponse {
    #[serde(default)]
    items: Option<Vec<RawPodcastIndexEpisode>>,
}

/// A usable episode (`PodcastIndexEpisode`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PodcastIndexEpisode {
    pub title: String,
    pub feed_title: String,
    pub feed_url: String,
    /// Omitted when the feed's iTunes id is 0/absent.
    pub feed_itunes_id: Option<i64>,
    pub guid: String,
    pub enclosure_url: String,
    pub episode_url: Option<String>,
    /// Epoch ms (the API sends seconds).
    pub published_at: i64,
    /// Rounded minutes; omitted when duration is absent or 0.
    pub duration_minutes: Option<i64>,
    pub description: String,
    /// `image`, falling back to `feedImage`.
    pub artwork_url: Option<String>,
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

fn js_round(x: f64) -> i64 {
    // Math.round: halves round toward +infinity.
    (x + 0.5).floor() as i64
}

/// `None` (skip) when feed URL, enclosure, publish date or guid
/// is missing; the guid forms the episode identity.
pub fn map_episode(raw: &RawPodcastIndexEpisode) -> Option<PodcastIndexEpisode> {
    let feed_url = non_empty(&raw.feed_url)?;
    let enclosure_url = non_empty(&raw.enclosure_url)?;
    let date_published = raw.date_published.filter(|d| *d != 0.0 && !d.is_nan())?;
    let guid = non_empty(&raw.guid)?;
    Some(PodcastIndexEpisode {
        title: raw.title.clone().unwrap_or_default(),
        feed_title: raw.feed_title.clone().unwrap_or_default(),
        feed_url: feed_url.to_owned(),
        feed_itunes_id: raw
            .feed_itunes_id
            .filter(|id| *id != 0.0 && !id.is_nan())
            .map(|id| id as i64),
        guid: guid.to_owned(),
        enclosure_url: enclosure_url.to_owned(),
        episode_url: non_empty(&raw.link).map(str::to_owned),
        published_at: (date_published * 1000.0) as i64,
        duration_minutes: raw
            .duration
            .filter(|d| *d != 0.0 && !d.is_nan())
            .map(|d| js_round(d / 60.0)),
        description: raw.description.clone().unwrap_or_default(),
        artwork_url: raw
            .image
            .clone()
            .or_else(|| raw.feed_image.clone())
            .filter(|a| !a.is_empty()),
    })
}

/// `PodcastIndexRequestError`.
#[derive(Debug, thiserror::Error)]
#[error("Podcast Index byperson request failed for {name}: {detail}")]
pub struct PodcastIndexRequestError {
    pub name: String,
    pub detail: String,
    transient: bool,
}

/// `PodcastIndexClient` (test seam for guest discovery).
pub trait PersonSearch: Send + Sync {
    fn search_by_person<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError>>;
}

/// The HTTP client, paced at 4 concurrent / 6 per second.
pub struct PodcastIndexClient {
    http: PublicHttpClient,
    credentials: PodcastIndexCredentials,
    clock: omni_core::clock::SharedClock,
    pacer: RequestPacer,
}

impl PodcastIndexClient {
    pub fn new(
        http: PublicHttpClient,
        credentials: PodcastIndexCredentials,
        clock: omni_core::clock::SharedClock,
    ) -> Self {
        Self {
            http,
            credentials,
            clock,
            pacer: RequestPacer::new(4, 6, Duration::from_secs(1)),
        }
    }

    /// `None` without both credentials (one half warns).
    pub fn from_config(
        config: &omni_config::Config,
        http: PublicHttpClient,
        clock: omni_core::clock::SharedClock,
    ) -> Option<Self> {
        let key = config.podcastindex_key.clone().filter(|s| !s.is_empty());
        let secret = config.podcastindex_secret.clone().filter(|s| !s.is_empty());
        match (key, secret) {
            (Some(key), Some(secret)) => Some(Self::new(
                http,
                PodcastIndexCredentials { key, secret },
                clock,
            )),
            (None, None) => None,
            _ => {
                tracing::warn!(target: LOG, "Podcast Index requires both PODCASTINDEX_KEY and PODCASTINDEX_SECRET");
                None
            }
        }
    }

    async fn attempt(&self, name: &str) -> Result<String, PodcastIndexRequestError> {
        let fail = |detail: String, transient: bool| PodcastIndexRequestError {
            name: name.to_owned(),
            detail,
            transient,
        };
        let _permit = self.pacer.acquire().await;
        let url = Url::parse(&format!("{BASE_URL}/search/byperson"))
            .map_err(|e| fail(e.to_string(), false))?;
        let mut request = self
            .http
            .request(Method::GET, url)
            .query(&[("q", name), ("max", DEFAULT_MAX_RESULTS)])
            .timeout(REQUEST_TIMEOUT);
        for (header, value) in podcast_index_auth_headers(&self.credentials, self.clock.now_ms()) {
            if header == "User-Agent" {
                // The shared public UA overrides the auth helper's.
                continue;
            }
            request = request.header(header, value);
        }
        request = request.header("User-Agent", omni_http::USER_AGENT);
        let response = request.send_bounded(RESPONSE_MAX_BYTES).await.map_err(|e| {
            let transient = matches!(e, HttpError::Timeout | HttpError::Network(_))
                || matches!(e, HttpError::Status { status, .. } if status == 429 || status >= 500);
            fail(e.to_string(), transient)
        })?;
        let status = response.status.as_u16();
        if !(200..300).contains(&status) {
            return Err(fail(
                format!("HTTP {status}"),
                status == 429 || status >= 500,
            ));
        }
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }

    pub async fn search_by_person(
        &self,
        name: &str,
    ) -> Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError> {
        let mut attempt = 0;
        let text = loop {
            match self.attempt(name).await {
                Ok(text) => break text,
                Err(e) if e.transient && attempt < RETRIES => {
                    tokio::time::sleep(RETRY_BASE * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        };
        parse_search_by_person(name, &text)
    }
}

/// Decodes a `search/byperson` body, skipping (and logging) unusable items.
pub fn parse_search_by_person(
    name: &str,
    text: &str,
) -> Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError> {
    let parsed: SearchByPersonResponse =
        serde_json::from_str(text).map_err(|e| PodcastIndexRequestError {
            name: name.to_owned(),
            detail: e.to_string(),
            transient: false,
        })?;
    let mut episodes = Vec::new();
    for raw in parsed.items.unwrap_or_default() {
        match map_episode(&raw) {
            Some(episode) => episodes.push(episode),
            None => tracing::debug!(
                target: LOG,
                title = raw.title.as_deref().unwrap_or_default(),
                "Skipping Podcast Index episode missing required fields"
            ),
        }
    }
    Ok(episodes)
}

impl PersonSearch for PodcastIndexClient {
    fn search_by_person<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PodcastIndexEpisode>, PodcastIndexRequestError>> {
        Box::pin(PodcastIndexClient::search_by_person(self, name))
    }
}
