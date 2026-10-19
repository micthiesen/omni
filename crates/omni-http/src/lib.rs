//! Outgoing HTTP: one reqwest client with the
//! project user agent, per-request timeouts and redirect rules, bounded
//! response bodies, and the SSRF-guarded public client.

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
pub use reqwest::{Method, Response, StatusCode};
pub use url::Url;

pub mod public;

use public::{AddressPolicy, PublicResolver};

/// Every client identifies as this (AGENTS.md); the Reminders Apple client
/// (`omni-reminders`) is the only documented exception.
pub const USER_AGENT: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";

/// Redirects followed by default by the public client.
pub const DEFAULT_PUBLIC_REDIRECTS: u8 = 10;

/// Client construction options.
#[derive(Clone, Debug, Default)]
pub struct HttpConfig {
    /// TCP connect timeout; `None` keeps reqwest's default (no timeout).
    pub connect_timeout: Option<Duration>,
    /// Refuse every DNS lookup (tests): only IP-literal URLs such as wiremock's
    /// `http://127.0.0.1:<port>` can be reached.
    pub offline: bool,
}

/// DNS resolver that refuses every name.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OfflineResolver;

impl reqwest::dns::Resolve for OfflineResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(
            async move { Err(format!("DNS lookups are disabled (offline client): {host}").into()) },
        )
    }
}

fn client_builder(cfg: &HttpConfig) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = cfg.connect_timeout {
        builder = builder.connect_timeout(timeout);
    }
    builder
}

/// Test-only base-URL rewrites: requests whose origin equals `from` are sent
/// to `to` instead (path and query preserved).
#[derive(Clone, Debug, Default)]
pub struct HttpOverrides {
    pub rewrites: Vec<(Url, Url)>,
}

/// The shared outgoing client. No default request timeout: every call site
/// sets its own.
#[derive(Clone, Debug)]
pub struct HttpClient {
    inner: reqwest::Client,
    /// The public-internet transport: same settings, every DNS answer must be
    /// a public address.
    public: reqwest::Client,
    /// The loopback-permitting public transport ([`public::PublicHttpClient::allow_loopback_for_tests`]).
    public_loopback: reqwest::Client,
    overrides: Arc<HttpOverrides>,
}

impl HttpClient {
    /// Builds the client: project UA, rustls, no default timeout, no automatic redirects
    /// (redirects follow [`RedirectRule`] per request).
    pub fn new(cfg: HttpConfig) -> Result<Self, HttpError> {
        let build = |builder: reqwest::ClientBuilder| {
            builder
                .build()
                .map_err(|e| HttpError::Network(error_chain(&e)))
        };
        let (inner, public, public_loopback) = if cfg.offline {
            (
                build(client_builder(&cfg).dns_resolver(OfflineResolver))?,
                build(client_builder(&cfg).dns_resolver(OfflineResolver))?,
                build(client_builder(&cfg).dns_resolver(OfflineResolver))?,
            )
        } else {
            (
                build(client_builder(&cfg))?,
                // A proxy from the environment would resolve names itself and
                // bypass the public-address check, so the public transports never use one.
                build(
                    client_builder(&cfg)
                        .no_proxy()
                        .dns_resolver(PublicResolver::system(AddressPolicy::PublicOnly)),
                )?,
                build(
                    client_builder(&cfg)
                        .no_proxy()
                        .dns_resolver(PublicResolver::system(AddressPolicy::AllowLoopback)),
                )?,
            )
        };
        Ok(Self {
            inner,
            public,
            public_loopback,
            overrides: Arc::new(HttpOverrides::default()),
        })
    }

    /// Applies test base-URL rewrites to every request built afterwards.
    pub fn with_overrides(self, o: HttpOverrides) -> Self {
        Self {
            overrides: Arc::new(o),
            ..self
        }
    }

    /// Starts a request; the URL is rewritten by any matching override.
    pub fn request(&self, m: Method, url: Url) -> RequestBuilder {
        self.request_on(&self.inner, m, url, Guard::None, RedirectRule::Error)
    }

    pub(crate) fn request_on(
        &self,
        client: &reqwest::Client,
        m: Method,
        url: Url,
        guard: Guard,
        redirect: RedirectRule,
    ) -> RequestBuilder {
        let url = rewrite(&self.overrides, url);
        RequestBuilder {
            inner: client.request(m, url.clone()),
            url,
            timeout: None,
            redirect,
            guard,
        }
    }

    pub(crate) fn public_transport(&self, policy: AddressPolicy) -> &reqwest::Client {
        match policy {
            AddressPolicy::PublicOnly => &self.public,
            AddressPolicy::AllowLoopback => &self.public_loopback,
        }
    }

    /// The underlying client, for streaming bodies (CalDAV, webhooks).
    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }
}

fn rewrite(overrides: &HttpOverrides, url: Url) -> Url {
    for (from, to) in &overrides.rewrites {
        if url.origin() == from.origin() {
            let mut rewritten = to.clone();
            let base_path = to.path().trim_end_matches('/');
            rewritten.set_path(&format!("{base_path}{}", url.path()));
            rewritten.set_query(url.query());
            rewritten.set_fragment(url.fragment());
            return rewritten;
        }
    }
    url
}

/// How a request treats a 3xx response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectRule {
    /// A redirect is an error (default).
    Error,
    /// Return the 3xx response as-is.
    None,
    /// Follow up to `n` redirects (re-validated for public requests).
    Follow(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Guard {
    None,
    /// Every URL, DNS answer and redirect hop must be public.
    Public(AddressPolicy),
}

/// A request in construction; wraps `reqwest::RequestBuilder`.
#[derive(Debug)]
pub struct RequestBuilder {
    inner: reqwest::RequestBuilder,
    url: Url,
    timeout: Option<Duration>,
    redirect: RedirectRule,
    guard: Guard,
}

impl RequestBuilder {
    /// Whole-request timeout (headers and body).
    pub fn timeout(self, d: Duration) -> Self {
        Self {
            inner: self.inner.timeout(d),
            timeout: Some(d),
            ..self
        }
    }

    pub fn header<K, V>(self, k: K, v: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        Self {
            inner: self.inner.header(k, v),
            ..self
        }
    }

    pub fn json<T: Serialize>(self, t: &T) -> Self {
        Self {
            inner: self.inner.json(t),
            ..self
        }
    }

    pub fn form(self, f: &[(&str, &str)]) -> Self {
        Self {
            inner: self.inner.form(f),
            ..self
        }
    }

    pub fn query(self, q: &[(&str, &str)]) -> Self {
        Self {
            inner: self.inner.query(q),
            ..self
        }
    }

    pub fn bearer_auth(self, token: &str) -> Self {
        Self {
            inner: self.inner.bearer_auth(token),
            ..self
        }
    }

    pub fn basic_auth(self, user: &str, password: Option<&str>) -> Self {
        Self {
            inner: self.inner.basic_auth(user, password),
            ..self
        }
    }

    pub fn body(self, body: impl Into<reqwest::Body>) -> Self {
        Self {
            inner: self.inner.body(body),
            ..self
        }
    }

    pub fn redirect(self, p: RedirectRule) -> Self {
        Self {
            redirect: p,
            ..self
        }
    }

    /// The target URL after override rewriting.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Sends and reads at most `max_bytes` of body (Content-Length precheck,
    /// then a streamed count). Non-2xx statuses are returned, not errors,
    /// except a redirect under [`RedirectRule::Error`]. The timeout covers
    /// every redirect hop and the whole body.
    pub async fn send_bounded(self, max_bytes: usize) -> Result<BoundedResponse, HttpError> {
        match self.timeout {
            Some(limit) => tokio::time::timeout(limit, self.execute(max_bytes))
                .await
                .map_err(|_| HttpError::Timeout)?,
            None => self.execute(max_bytes).await,
        }
    }

    async fn execute(self, max_bytes: usize) -> Result<BoundedResponse, HttpError> {
        let (client, request) = self.inner.build_split();
        let mut request = request.map_err(|e| map_reqwest(&e))?;
        let mut hops: u8 = 0;
        loop {
            if let Guard::Public(policy) = self.guard {
                public::check_url(request.url().as_str(), policy)?;
            }
            let replay = request.try_clone();
            let method = request.method().clone();
            let headers = request.headers().clone();
            let response = client.execute(request).await.map_err(|e| map_reqwest(&e))?;
            let status = response.status();
            let location = redirect_location(&response);
            let Some(location) = location else {
                return read_bounded(response, max_bytes).await;
            };
            match self.redirect {
                RedirectRule::None => return read_bounded(response, max_bytes).await,
                RedirectRule::Error => {
                    let body = read_bounded(response, max_bytes).await?.body;
                    return Err(HttpError::Status {
                        status: status.as_u16(),
                        body: truncate_body(&body),
                    });
                }
                RedirectRule::Follow(max) if hops >= max => {
                    return Err(HttpError::Blocked(format!(
                        "too many redirects (more than {max})"
                    )));
                }
                RedirectRule::Follow(_) => {}
            }
            let from = response.url().clone();
            let next = from.join(&location).map_err(|e| {
                HttpError::InvalidUrl(format!("redirect location {location:?}: {e}"))
            })?;
            request = redirected_request(status, method, headers, replay, &from, next)?;
            hops += 1;
        }
    }

    /// Sends without reading the body, for streamed responses (Server-Sent
    /// Events). Redirects are never followed: under [`RedirectRule::Error`] a
    /// 3xx is `HttpError::Status`, otherwise it is returned. A public request
    /// keeps its URL check and DNS guard. The timeout, when set, covers only
    /// the response headers; the caller bounds the body.
    pub async fn send_streaming(self) -> Result<reqwest::Response, HttpError> {
        let (client, request) = self.inner.build_split();
        let mut request = request.map_err(|e| map_reqwest(&e))?;
        // reqwest's whole-request timeout would also cut a long stream.
        *request.timeout_mut() = None;
        if let Guard::Public(policy) = self.guard {
            public::check_url(request.url().as_str(), policy)?;
        }
        let send = client.execute(request);
        let response = match self.timeout {
            Some(limit) => tokio::time::timeout(limit, send)
                .await
                .map_err(|_| HttpError::Timeout)?,
            None => send.await,
        }
        // The URL may carry a credential in its query; errors never repeat it.
        .map_err(|e| map_reqwest(&e.without_url()))?;
        if self.redirect == RedirectRule::Error && redirect_location(&response).is_some() {
            return Err(HttpError::Status {
                status: response.status().as_u16(),
                body: String::new(),
            });
        }
        Ok(response)
    }

    /// [`Self::send_bounded`] then JSON decode; non-2xx becomes `HttpError::Status`.
    pub async fn json_bounded<T: DeserializeOwned>(self, max_bytes: usize) -> Result<T, HttpError> {
        let response = self.send_bounded(max_bytes).await?;
        if !response.status.is_success() {
            return Err(HttpError::Status {
                status: response.status.as_u16(),
                body: truncate_body(&response.body),
            });
        }
        serde_json::from_slice(&response.body).map_err(|e| HttpError::Decode(e.to_string()))
    }
}

/// The `Location` of a redirect response (301, 302, 303, 307, 308). A 304 or
/// a 3xx without `Location` is an ordinary response.
fn redirect_location(response: &reqwest::Response) -> Option<String> {
    let redirect = matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308);
    if !redirect {
        return None;
    }
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The next hop: 303 (and 301/302 after POST) becomes a bodiless GET, 307/308
/// replay the method and body. Credentials are not forwarded across origins.
fn redirected_request(
    status: StatusCode,
    method: Method,
    mut headers: HeaderMap,
    replay: Option<reqwest::Request>,
    from: &Url,
    next: Url,
) -> Result<reqwest::Request, HttpError> {
    use reqwest::header::{
        AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, PROXY_AUTHORIZATION, TRANSFER_ENCODING,
    };
    let to_get = match status.as_u16() {
        303 => method != Method::HEAD,
        301 | 302 => method == Method::POST,
        _ => false,
    };
    if from.origin() != next.origin() {
        headers.remove(AUTHORIZATION);
        headers.remove(PROXY_AUTHORIZATION);
        headers.remove(COOKIE);
    }
    if to_get {
        headers.remove(CONTENT_TYPE);
        headers.remove(CONTENT_LENGTH);
        headers.remove(TRANSFER_ENCODING);
        let mut request = reqwest::Request::new(Method::GET, next);
        *request.headers_mut() = headers;
        return Ok(request);
    }
    let mut request = match replay {
        Some(replay) => replay,
        None => {
            return Err(HttpError::Blocked(
                "redirect requires replaying a streaming request body".to_owned(),
            ));
        }
    };
    *request.url_mut() = next;
    *request.headers_mut() = headers;
    Ok(request)
}

/// Reads the body while it stays within `max_bytes`; a declared oversize
/// length is rejected before reading anything.
async fn read_bounded(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<BoundedResponse, HttpError> {
    let limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    if response
        .content_length()
        .is_some_and(|declared| declared > limit)
    {
        return Err(HttpError::TooLarge { limit: max_bytes });
    }
    let status = response.status();
    let headers = response.headers().clone();
    let final_url = response.url().clone();
    let mut body = BytesMut::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| map_reqwest(&e))? {
        if body.len() + chunk.len() > max_bytes {
            return Err(HttpError::TooLarge { limit: max_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(BoundedResponse {
        status,
        headers,
        body: body.freeze(),
        final_url,
    })
}

/// `error` followed by its source chain, `: `-separated.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

fn map_reqwest(error: &reqwest::Error) -> HttpError {
    if error.is_timeout() {
        return HttpError::Timeout;
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(cause) = source {
        if let Some(blocked) = cause.downcast_ref::<public::NonPublicAddress>() {
            return HttpError::Blocked(blocked.to_string());
        }
        source = cause.source();
    }
    if error.is_builder() {
        return HttpError::InvalidUrl(error_chain(error));
    }
    HttpError::Network(error_chain(error))
}

/// Error bodies are kept to 4 KiB (lossy UTF-8, cut at a char boundary).
fn truncate_body(body: &[u8]) -> String {
    const LIMIT: usize = 4096;
    let text = String::from_utf8_lossy(&body[..body.len().min(LIMIT)]).into_owned();
    if body.len() > LIMIT {
        format!("{text}…")
    } else {
        text
    }
}

/// A fully read, size-bounded response.
#[derive(Clone, Debug)]
pub struct BoundedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub final_url: Url,
}

#[derive(thiserror::Error, Debug)]
pub enum HttpError {
    #[error("request timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("response body exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
}

impl HttpError {
    /// `Timeout | Network | 5xx | 408 | 429`.
    pub fn is_transient(&self) -> bool {
        match self {
            HttpError::Timeout | HttpError::Network(_) => true,
            HttpError::Status { status, .. } => *status >= 500 || *status == 408 || *status == 429,
            _ => false,
        }
    }
}

/// Whether mutating adapters perform side effects or only record them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideEffectMode {
    Live,
    /// Compat shadow runs and tests: every mutating adapter records instead of sending.
    Record,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_classification() {
        assert!(HttpError::Timeout.is_transient());
        assert!(
            HttpError::Status {
                status: 503,
                body: String::new()
            }
            .is_transient()
        );
        assert!(
            HttpError::Status {
                status: 429,
                body: String::new()
            }
            .is_transient()
        );
        assert!(
            !HttpError::Status {
                status: 404,
                body: String::new()
            }
            .is_transient()
        );
        assert!(!HttpError::TooLarge { limit: 1 }.is_transient());
    }

    #[test]
    fn overrides_rewrite_origin() -> Result<(), Box<dyn std::error::Error>> {
        let client = HttpClient::new(HttpConfig::default())?.with_overrides(HttpOverrides {
            rewrites: vec![(
                Url::parse("https://api.pushover.net")?,
                Url::parse("http://127.0.0.1:9/mock/")?,
            )],
        });
        let req = client.request(
            Method::POST,
            Url::parse("https://api.pushover.net/1/messages.json?a=1")?,
        );
        assert_eq!(
            req.url().as_str(),
            "http://127.0.0.1:9/mock/1/messages.json?a=1"
        );
        Ok(())
    }
}
