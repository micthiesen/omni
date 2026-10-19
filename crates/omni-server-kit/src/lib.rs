//! axum building blocks shared by every router.

use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

use futures::FutureExt as _;

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequest, Request};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::Route;
use omni_api::common::ApiErrorBody;
use serde::de::DeserializeOwned;
use tower::{Layer, Service};

pub mod public_host;
pub mod sse;

/// Default JSON request body cap (64 KiB).
pub const JSON_BODY_LIMIT: usize = 64 * 1024;

/// `{"error": msg}` with `status`.
pub fn api_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(ApiErrorBody::new(msg))).into_response()
}

const LOG: &str = "Server";

/// The opaque error response: 500 `text/plain` "Internal Server Error".
pub fn internal_server_error() -> Response {
    let mut response = Response::new(Body::from("Internal Server Error"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    response
}

/// A handler failure. Expected failures carry their status and message and
/// render as `{"error": msg}`; [`ApiError::internal`] logs the cause at ERROR
/// and renders the opaque 500.
#[derive(Debug)]
pub enum ApiError {
    /// `{"error": message}` with `status`.
    Status { status: StatusCode, message: String },
    /// Unhandled failure; the cause is logged, never returned to the client.
    Internal(String),
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError::Status {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// An unexpected failure (store, I/O, defect).
    pub fn internal(error: impl std::fmt::Display) -> Self {
        ApiError::Internal(error.to_string())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status { status, message } => write!(f, "{status}: {message}"),
            ApiError::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            ApiError::Status { status, message } => api_error(status, message),
            ApiError::Internal(message) => {
                tracing::error!(target: LOG, error = %message, "Request failed");
                internal_server_error()
            }
        }
    }
}

/// `Result` alias for JSON handlers.
pub type ApiResult<T> = Result<axum::Json<T>, ApiError>;

async fn catch_panic_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    match AssertUnwindSafe(next.run(req)).catch_unwind().await {
        Ok(response) => response,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "handler panicked".to_owned());
            tracing::error!(target: LOG, %method, %path, panic = %message, "Request handler panicked");
            internal_server_error()
        }
    }
}

/// Converts a panicking handler into the opaque 500 response (logged at ERROR)
/// instead of dropping the connection.
pub fn catch_panic_layer() -> impl Layer<
    Route,
    Service: Service<Request, Response = Response, Error = Infallible, Future: Send + 'static>
                 + Clone
                 + Send
                 + Sync
                 + 'static,
> + Clone
+ Send
+ Sync
+ 'static {
    axum::middleware::from_fn(catch_panic_middleware)
}

/// Composes the application router: merges the parts (each already at its
/// absolute paths with state applied), applies the `/api/*` same-origin
/// guard and panic catching, and serves the SPA (or a JSON 404 for unknown
/// `/api/*` paths when no frontend is built) as the fallback.
pub fn app_router(parts: impl IntoIterator<Item = Router>, web_dist: Option<PathBuf>) -> Router {
    let mut router = Router::new();
    for part in parts {
        router = router.merge(part);
    }
    let router = match web_dist {
        Some(dist) => router.fallback_service(spa_service(dist)),
        None => router.fallback(|| async { api_error(StatusCode::NOT_FOUND, "Not found") }),
    };
    router
        .layer(same_origin_mutation_guard())
        .layer(catch_panic_layer())
}

/// JSON body capped at 64 KiB (Content-Length precheck plus a streamed cap).
/// Oversize bodies get 413 `{"error":"Request body too large"}`; invalid JSON
/// is treated as `null`, so decoding `T` fails with 400 `{"error": <message>}`.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonBody<T>(pub T);

/// [`JsonBody`] with a custom cap (device-link results use 512 KiB).
#[derive(Clone, Debug, PartialEq)]
pub struct JsonBodyLimit<const N: usize, T>(pub T);

async fn read_json<T: DeserializeOwned>(req: Request, limit: usize) -> Result<T, Response> {
    let too_large = || api_error(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| too_large())?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    serde_json::from_value(value).map_err(|e| api_error(StatusCode::BAD_REQUEST, e.to_string()))
}

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        read_json(req, JSON_BODY_LIMIT).await.map(JsonBody)
    }
}

impl<S, T, const N: usize> FromRequest<S> for JsonBodyLimit<N, T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, _state: &S) -> Result<Self, Self::Rejection> {
        read_json(req, N).await.map(JsonBodyLimit)
    }
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// `new URL(origin).host`: host plus a non-default port.
fn origin_host(origin: &str) -> Option<String> {
    let url = url::Url::parse(origin).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

/// The `/api/*` rule: a non-GET/HEAD request with an `Origin` must match
/// `Host` (403 `Cross-origin mutations are not allowed`); successful API
/// responses get `X-Content-Type-Options: nosniff`.
pub async fn same_origin_mutation_middleware(req: Request, next: Next) -> Response {
    if !is_api_path(req.uri().path()) {
        return next.run(req).await;
    }
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let headers = req.headers();
        if let Some(origin) = headers.get(ORIGIN) {
            let host = headers
                .get(HOST)
                .and_then(|h| h.to_str().ok())
                .filter(|h| !h.is_empty());
            let same = match (origin.to_str().ok().and_then(origin_host), host) {
                (Some(origin_host), Some(host)) => origin_host == host,
                _ => false,
            };
            if !same {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "Cross-origin mutations are not allowed",
                );
            }
        }
    }
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// [`same_origin_mutation_middleware`] as a router layer.
pub fn same_origin_mutation_guard() -> impl Layer<
    Route,
    Service: Service<Request, Response = Response, Error = Infallible, Future: Send + 'static>
                 + Clone
                 + Send
                 + Sync
                 + 'static,
> + Clone
+ Send
+ Sync
+ 'static {
    axum::middleware::from_fn(same_origin_mutation_middleware)
}

/// Constant-time bearer check: `^Bearer (\S+)$` (case-insensitive scheme),
/// both sides hashed with SHA-256 before comparison.
pub fn bearer_digest_eq(header: Option<&HeaderValue>, expected: &str) -> bool {
    let supplied = header
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, token) = value.split_at_checked(7)?;
            (scheme.eq_ignore_ascii_case("bearer ")
                && !token.is_empty()
                && !token.chars().any(char::is_whitespace))
            .then_some(token)
        })
        .unwrap_or("");
    omni_core::digest::ct_eq_sha256(supplied.as_bytes(), expected.as_bytes())
}

/// `/reminders` page headers; the CSP adds `'wasm-unsafe-eval'` for the Leptos bundle.
pub const REMINDERS_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

async fn reminders_headers_middleware(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(REMINDERS_CSP),
    );
    response
}

/// Layer applying the `/reminders` headers (Cache-Control, Referrer-Policy, CSP).
pub fn reminders_page_headers() -> impl Layer<
    Route,
    Service: Service<Request, Response = Response, Error = Infallible, Future: Send + 'static>
                 + Clone
                 + Send
                 + Sync
                 + 'static,
> + Clone
+ Send
+ Sync
+ 'static {
    axum::middleware::from_fn(reminders_headers_middleware)
}

async fn spa_cache_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    if method != Method::GET && method != Method::HEAD {
        return api_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    }
    if is_api_path(&path) {
        return api_error(StatusCode::NOT_FOUND, "Not found");
    }
    let mut response = next.run(req).await;
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    // A missing asset falls back to index.html; answer 404 instead so a browser
    // never caches HTML under an immutable asset path.
    if path.starts_with("/assets/") && is_html {
        let mut missing = Response::new(Body::from("Not found"));
        *missing.status_mut() = StatusCode::NOT_FOUND;
        missing
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return missing;
    }
    let cache = if path.starts_with("/assets/") {
        Some("public, max-age=31536000, immutable")
    } else if is_html {
        Some("no-cache")
    } else {
        None
    };
    if let Some(cache) = cache {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    }
    response
}

/// The built frontend: precompressed (br, gzip) static files, `index.html`
/// for client routes, immutable `/assets/*`, `no-cache` HTML, and a JSON 404
/// for unknown `/api/*` paths.
pub fn spa_service(dist: PathBuf) -> Router {
    let index = dist.join("index.html");
    let files = tower_http::services::ServeDir::new(&dist)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(tower_http::services::ServeFile::new(index));
    Router::new()
        .fallback_service(files)
        .layer(axum::middleware::from_fn(spa_cache_middleware))
}

/// Body helper for handlers that already hold a serialized payload.
pub fn json_response(status: StatusCode, json: String) -> Response {
    let mut response = Response::new(Body::from(json));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use tower::ServiceExt as _;

    #[derive(serde::Deserialize)]
    struct Input {
        name: String,
    }

    fn app() -> Router {
        Router::new()
            .route(
                "/api/echo",
                post(|JsonBody(input): JsonBody<Input>| async move { input.name }),
            )
            .layer(same_origin_mutation_guard())
    }

    async fn send(req: Request) -> Response {
        match app().oneshot(req).await {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    fn post_json(body: &str, origin: Option<&str>) -> Request {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/api/echo")
            .header(HOST, "omni.boris")
            .header(CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            builder = builder.header(ORIGIN, origin);
        }
        builder
            .body(Body::from(body.to_owned()))
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn guard_and_body_rules() {
        let ok = send(post_json(r#"{"name":"x"}"#, Some("http://omni.boris"))).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers()
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            send(post_json(r#"{"name":"x"}"#, Some("http://evil.example")))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(post_json(r#"{"name":"x"}"#, None)).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            send(post_json("not json", None)).await.status(),
            StatusCode::BAD_REQUEST
        );
        let big = format!(r#"{{"name":"{}"}}"#, "a".repeat(JSON_BODY_LIMIT));
        assert_eq!(
            send(post_json(&big, None)).await.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn bearer_rules() {
        let header = |v: &str| HeaderValue::from_str(v).ok();
        assert!(bearer_digest_eq(header("Bearer secret").as_ref(), "secret"));
        assert!(bearer_digest_eq(header("bearer secret").as_ref(), "secret"));
        assert!(!bearer_digest_eq(
            header("Bearer  secret").as_ref(),
            "secret"
        ));
        assert!(!bearer_digest_eq(header("Basic secret").as_ref(), "secret"));
        assert!(!bearer_digest_eq(None, "secret"));
    }

    #[tokio::test]
    async fn missing_assets_are_not_served_as_cached_html() {
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("index.html"), "<html></html>").unwrap();
        std::fs::create_dir(dist.path().join("assets")).unwrap();
        std::fs::write(dist.path().join("assets/app.css"), "body{}").unwrap();
        let get = |uri: &str| {
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .unwrap_or_default()
        };
        let router = spa_service(dist.path().to_path_buf());
        let missing = router
            .clone()
            .oneshot(get("/assets/fonts/none.woff2"))
            .await;
        let missing = missing.unwrap_or_else(|never| match never {});
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[CACHE_CONTROL], "no-store");
        let found = router.clone().oneshot(get("/assets/app.css")).await;
        let found = found.unwrap_or_else(|never| match never {});
        assert_eq!(found.status(), StatusCode::OK);
        assert_eq!(
            found.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let route = router.oneshot(get("/streamers/x")).await;
        let route = route.unwrap_or_else(|never| match never {});
        assert_eq!(route.status(), StatusCode::OK);
        assert_eq!(route.headers()[CACHE_CONTROL], "no-cache");
    }
}
