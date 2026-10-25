//! Radarr/Sonarr v3 API access.
//!
//! Every request is bounded (15 s overall and an 8 MiB body unless the call
//! names wider [`Limits`]) and every failure, malformed payload included, reads
//! as `Unavailable`: callers never mistake a partial or broken response for the
//! tracked state.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_http::{HttpClient, Method, RedirectRule, SideEffectMode, Url};
use serde::de::DeserializeOwned;

pub mod inspect;
pub mod movies;
pub mod queue;
pub mod radarr;
pub mod seasons;
pub mod sonarr;

pub const ARR_JSON_MAX_BYTES: usize = 8 * 1024 * 1024;
const ARR_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-request bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub max_bytes: usize,
}

impl Limits {
    pub const DEFAULT: Self = Self {
        timeout: ARR_TIMEOUT,
        max_bytes: ARR_JSON_MAX_BYTES,
    };
    /// Whole-library listings: Radarr's movie list runs to several MiB.
    pub const LIBRARY: Self = Self {
        timeout: Duration::from_secs(30),
        max_bytes: 48 * 1024 * 1024,
    };
    /// Interactive release searches wait for every indexer.
    pub const INDEXER_SEARCH: Self = Self {
        timeout: Duration::from_secs(120),
        max_bytes: 16 * 1024 * 1024,
    };
}
/// `fetch` follows up to 20 redirects.
const MAX_REDIRECTS: u8 = 20;
const LOG: &str = "Recommendations";

/// One service's connection and acquisition defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArrConfig {
    pub url: Option<String>,
    pub api_key: Option<String>,
    pub root_folder_path: Option<String>,
    pub quality_profile_id: Option<u64>,
}

impl ArrConfig {
    /// A URL and an API key.
    pub fn connection(&self) -> Option<(&str, &str)> {
        let url = self.url.as_deref().filter(|u| !u.is_empty())?;
        let key = self.api_key.as_deref().filter(|k| !k.is_empty())?;
        Some((url, key))
    }

    /// Connection plus root folder and quality profile.
    pub fn acquisition(&self) -> Option<(&str, u64)> {
        self.connection()?;
        let root = self.root_folder_path.as_deref().filter(|r| !r.is_empty())?;
        Some((root, self.quality_profile_id?))
    }
}

/// `HttpResult`.
#[derive(Clone, Debug, PartialEq)]
pub enum HttpResult<T> {
    Ok(T),
    HttpError { status_code: u16 },
    Unavailable,
}

/// What a write's response proves about whether Sonarr or Radarr applied it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Accepted,
    /// A timeout, unreadable body or 5xx: the write may have been applied.
    Uncertain,
    /// A 4xx: the service refused the write.
    Rejected,
}

impl WriteOutcome {
    pub fn of<T>(result: &HttpResult<T>) -> Self {
        match result {
            HttpResult::Ok(_) => Self::Accepted,
            HttpResult::HttpError { status_code } if *status_code < 500 => Self::Rejected,
            _ => Self::Uncertain,
        }
    }
}

/// A POST body; `None` sends a GET.
pub type JsonBody = Option<serde_json::Value>;

/// A write recorded instead of sent (`SideEffectMode::Record`).
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedArrWrite {
    pub url: String,
    pub body: serde_json::Value,
}

/// HTTP access shared by the Radarr and Sonarr adapters.
#[derive(Clone)]
pub struct ArrHttp {
    http: HttpClient,
    mode: SideEffectMode,
    recorded: Arc<Mutex<Vec<RecordedArrWrite>>>,
}

impl ArrHttp {
    pub fn new(http: HttpClient, mode: SideEffectMode) -> Self {
        Self {
            http,
            mode,
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Writes recorded in `SideEffectMode::Record`.
    pub fn recorded(&self) -> Vec<RecordedArrWrite> {
        self.recorded
            .lock()
            .map(|writes| writes.clone())
            .unwrap_or_default()
    }

    pub fn mode(&self) -> SideEffectMode {
        self.mode
    }

    /// `<url>/api/v3/<path>` with the API key; any failure,
    /// timeout or schema mismatch is `Unavailable`, a non-2xx is `HttpError`.
    pub async fn request_json<T: DeserializeOwned>(
        &self,
        (base, api_key): (&str, &str),
        path: &str,
        body: JsonBody,
    ) -> HttpResult<T> {
        let method = if body.is_some() {
            Method::POST
        } else {
            Method::GET
        };
        self.send_json((base, api_key), method, path, body).await
    }

    /// [`Self::request_json`] with an explicit method (Sonarr's series update is a PUT).
    pub async fn send_json<T: DeserializeOwned>(
        &self,
        (base, api_key): (&str, &str),
        method: Method,
        path: &str,
        body: JsonBody,
    ) -> HttpResult<T> {
        self.send_limited((base, api_key), method, path, body, Limits::DEFAULT)
            .await
    }

    /// [`Self::send_json`] under explicit [`Limits`].
    pub async fn send_limited<T: DeserializeOwned>(
        &self,
        (base, api_key): (&str, &str),
        method: Method,
        path: &str,
        body: JsonBody,
        limits: Limits,
    ) -> HttpResult<T> {
        match tokio::time::timeout(
            limits.timeout,
            self.request(base, api_key, method, path, body, limits.max_bytes),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::debug!(target: LOG, "Arr request {path} timed out");
                HttpResult::Unavailable
            }
        }
    }

    /// [`Self::send_limited`] for a write whose response body is empty or
    /// irrelevant (Radarr's and Sonarr's `DELETE` answer 200 with no body).
    pub async fn send_unit(
        &self,
        (base, api_key): (&str, &str),
        method: Method,
        path: &str,
        limits: Limits,
    ) -> HttpResult<()> {
        match tokio::time::timeout(
            limits.timeout,
            self.request_bytes(base, api_key, method, path, None, limits.max_bytes),
        )
        .await
        {
            Ok(result) => match result {
                HttpResult::Ok(_) => HttpResult::Ok(()),
                HttpResult::HttpError { status_code } => HttpResult::HttpError { status_code },
                HttpResult::Unavailable => HttpResult::Unavailable,
            },
            Err(_) => {
                tracing::debug!(target: LOG, "Arr request {path} timed out");
                HttpResult::Unavailable
            }
        }
    }

    async fn request<T: DeserializeOwned>(
        &self,
        base: &str,
        api_key: &str,
        method: Method,
        path: &str,
        body: JsonBody,
        max_bytes: usize,
    ) -> HttpResult<T> {
        let body = match self
            .request_bytes(base, api_key, method, path, body, max_bytes)
            .await
        {
            HttpResult::Ok(response) => response.body,
            HttpResult::HttpError { status_code } => return HttpResult::HttpError { status_code },
            HttpResult::Unavailable => return HttpResult::Unavailable,
        };
        match serde_json::from_slice::<T>(&body) {
            Ok(value) => HttpResult::Ok(value),
            Err(e) => {
                tracing::debug!(target: LOG, "Arr response {path} rejected: {e}");
                HttpResult::Unavailable
            }
        }
    }

    async fn request_bytes(
        &self,
        base: &str,
        api_key: &str,
        method: Method,
        path: &str,
        body: JsonBody,
        max_bytes: usize,
    ) -> HttpResult<omni_http::BoundedResponse> {
        let Some(url) = arr_url(base, path) else {
            return HttpResult::Unavailable;
        };
        let mut request = self
            .http
            .request(method, url)
            .header("Accept", "application/json")
            .header("X-Api-Key", api_key)
            .redirect(RedirectRule::Follow(MAX_REDIRECTS));
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(omni_core::js::json_stringify(&body));
        }
        let response = match request.send_bounded(max_bytes).await {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(target: LOG, "Arr request {path} failed: {e}");
                return HttpResult::Unavailable;
            }
        };
        if !response.status.is_success() {
            return HttpResult::HttpError {
                status_code: response.status.as_u16(),
            };
        }
        HttpResult::Ok(response)
    }

    /// Records a write instead of sending it (Record mode only).
    pub(crate) fn record_write(&self, base: &str, path: &str, body: serde_json::Value) {
        let url = arr_url(base, path)
            .map(|u| u.to_string())
            .unwrap_or_default();
        tracing::info!(target: LOG, "Recorded Arr write {url} (side effects recorded, not sent)");
        if let Ok(mut writes) = self.recorded.lock() {
            writes.push(RecordedArrWrite { url, body });
        }
    }
}

/// `new URL("api/v3/<path>", "<base without trailing slashes>/")`.
pub fn arr_url(base: &str, path: &str) -> Option<Url> {
    let base = Url::parse(&format!("{}/", base.trim_end_matches('/'))).ok()?;
    base.join(&format!(
        "api/v3/{}",
        path.strip_prefix('/').unwrap_or(path)
    ))
    .ok()
}

/// Inserts `key` only when `value` is present (`JSON.stringify` drops `undefined`).
pub(crate) fn put_opt<T: serde::Serialize>(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value.and_then(|v| serde_json::to_value(v).ok()) {
        object.insert(key.to_owned(), value);
    }
}
