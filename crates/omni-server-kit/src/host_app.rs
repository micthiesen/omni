//! A second web app served on its own host by the one listener.
//!
//! Omni learns which host a request is for only from `Host`. A [`HostApp`]
//! pairs one host with a router of its own, and [`dispatch`] sends every
//! request for that host to that router and nowhere else: on the app's host
//! nothing of Omni exists (no UI, no `/api/*`, no `/gateway`), and on every
//! other host the app does not exist. The app's router is never merged into
//! Omni's.
//!
//! [`MutationGuard`] is the cross-site request rule for such an app, which has
//! no login of its own: see [`guard_mutations`].

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use tower::ServiceExt as _;

use crate::public_host::request_host;

/// The origin given to [`HostApp::new`] is not a plain `https://` origin (or
/// a plain `http://` origin on `localhost` or a `.localhost` name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidAppOrigin;

impl std::fmt::Display for InvalidAppOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "expected an https:// origin (or http:// on localhost) without credentials, path, query or fragment",
        )
    }
}

impl std::error::Error for InvalidAppOrigin {}

/// One host and the only router that answers on it.
#[derive(Clone)]
pub struct HostApp {
    host: Arc<str>,
    origin: Arc<str>,
    router: Router,
}

impl HostApp {
    /// `origin` is the app's HTTPS origin (`https://recipes.example.com`);
    /// `router` holds its routes at absolute paths with state applied. Plain
    /// HTTP is accepted only for `localhost` and `<name>.localhost`, which
    /// never leave the machine (local runs).
    ///
    /// The router gets panic catching and a fixed fallback, replacing any it
    /// had: an unmatched path is a 404, as `{"error": "Not found"}` under
    /// `/api`. A host app has no catch-all page.
    pub fn new(origin: &str, router: Router) -> Result<Self, InvalidAppOrigin> {
        let url = url::Url::parse(origin).map_err(|_| InvalidAppOrigin)?;
        let local = url
            .host_str()
            .is_some_and(|host| host == "localhost" || host.ends_with(".localhost"));
        let plain = (url.scheme() == "https" || (url.scheme() == "http" && local))
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none();
        let host = url.host_str().filter(|_| plain).ok_or(InvalidAppOrigin)?;
        Ok(Self {
            host: Arc::from(host.trim_end_matches('.').to_ascii_lowercase()),
            origin: Arc::from(url.origin().ascii_serialization()),
            router: router.fallback(not_found).layer(crate::catch_panic_layer()),
        })
    }

    /// The host this app answers on: lowercase, without port or trailing dot.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The app's origin as a browser sends it in `Origin`.
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

async fn not_found(request: Request) -> Response {
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        return crate::api_error(StatusCode::NOT_FOUND, "Not found");
    }
    let mut response = Response::new(axum::body::Body::from("Not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    response
}

/// Middleware over the whole application: a request whose host (see
/// [`request_host`]) is a host app's goes to that app's router; any other
/// request continues unchanged. With two apps on one host the first wins.
pub async fn dispatch_to_host_apps(
    State(apps): State<Arc<[HostApp]>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request_host(&request);
    let app = host
        .as_deref()
        .and_then(|host| apps.iter().find(|app| &*app.host == host));
    match app {
        Some(app) => match app.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        },
        None => next.run(request).await,
    }
}

/// `router` with [`dispatch_to_host_apps`] applied. Apply it last, outside
/// every other layer, so nothing of `router` runs for an app's host.
pub fn dispatch(router: Router, apps: Vec<HostApp>) -> Router {
    if apps.is_empty() {
        return router;
    }
    router.layer(axum::middleware::from_fn_with_state(
        Arc::<[HostApp]>::from(apps),
        dispatch_to_host_apps,
    ))
}

/// The mutation rule of a host app (see [`guard_mutations`]).
#[derive(Clone)]
pub struct MutationGuard {
    origin: Arc<str>,
    header_paths: Option<HeaderPaths>,
}

/// Paths that take a custom header instead of a JSON content type.
#[derive(Clone)]
struct HeaderPaths {
    header: HeaderName,
    is_path: fn(&str) -> bool,
}

impl MutationGuard {
    /// A guard for the app at `origin` ([`HostApp::origin`]).
    pub fn new(origin: &str) -> Self {
        Self {
            origin: Arc::from(origin),
            header_paths: None,
        }
    }

    /// Paths for which `is_path` is true (uploads) need `header` to be
    /// present instead of a JSON content type. A page on another origin
    /// cannot send a custom header without a CORS preflight, which the app
    /// never answers.
    pub fn with_header_paths(mut self, header: HeaderName, is_path: fn(&str) -> bool) -> Self {
        self.header_paths = Some(HeaderPaths { header, is_path });
        self
    }

    /// `Err` with the reason when `request` is a mutation that must be refused.
    pub fn check(&self, request: &Request) -> Result<(), &'static str> {
        if request.method() == Method::GET || request.method() == Method::HEAD {
            return Ok(());
        }
        let header_str = |name: &HeaderName| {
            request
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap_or(""))
        };
        if header_str(&header::ORIGIN) != Some(&*self.origin) {
            return Err("Cross-origin requests are not allowed");
        }
        let fetch_site = header_str(&HeaderName::from_static("sec-fetch-site"));
        if fetch_site.is_some_and(|site| site != "same-origin") {
            return Err("Cross-origin requests are not allowed");
        }
        match &self.header_paths {
            Some(paths) if (paths.is_path)(request.uri().path()) => {
                if !request.headers().contains_key(&paths.header) {
                    return Err("Missing the upload header");
                }
            }
            _ => {
                let content_type = header_str(&header::CONTENT_TYPE)
                    .and_then(|value| value.split(';').next())
                    .map(|value| value.trim().to_ascii_lowercase());
                if content_type.as_deref() != Some("application/json") {
                    return Err("Expected application/json");
                }
            }
        }
        Ok(())
    }
}

/// Middleware for a host app's router: every request other than GET and HEAD
/// needs the exact app `Origin`, a `Sec-Fetch-Site` of `same-origin` when the
/// header is present, and `Content-Type: application/json` (or, on the
/// guard's header paths, its custom header). Anything else is a 403
/// `{"error": ...}` and the handler never runs.
///
/// ```ignore
/// let guard = MutationGuard::new(app_origin)
///     .with_header_paths(HeaderName::from_static("x-fond-upload"), is_upload_path);
/// router.layer(axum::middleware::from_fn_with_state(guard, guard_mutations))
/// ```
pub async fn guard_mutations(
    State(guard): State<MutationGuard>,
    request: Request,
    next: Next,
) -> Response {
    match guard.check(&request) {
        Ok(()) => next.run(request).await,
        Err(reason) => crate::api_error(StatusCode::FORBIDDEN, reason),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::routing::{get, post};

    use super::*;

    const ORIGIN: &str = "https://recipes.example.com";

    fn app() -> HostApp {
        let guard = MutationGuard::new(ORIGIN)
            .with_header_paths(HeaderName::from_static("x-fond-upload"), |path| {
                path == "/api/upload"
            });
        let router = Router::new()
            .route("/", get(|| async { "app" }))
            .route("/api/notes", post(|| async { "saved" }))
            .route("/api/upload", post(|| async { "stored" }))
            .route("/boom", get(|| async { panic_now() }))
            .layer(axum::middleware::from_fn_with_state(guard, guard_mutations));
        HostApp::new(ORIGIN, router).unwrap()
    }

    #[allow(clippy::panic)]
    fn panic_now() -> &'static str {
        panic!("handler bug")
    }

    fn omni() -> Router {
        dispatch(
            Router::new()
                .route("/", get(|| async { "omni" }))
                .route("/api/health", get(|| async { "ok" }))
                .fallback(|| async { "omni spa" }),
            vec![app()],
        )
    }

    async fn send(request: Request) -> (StatusCode, String) {
        let response = omni().oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn get_from(host: &str, path: &str) -> Request {
        Request::get(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap()
    }

    fn post_to(path: &str, headers: &[(&str, &str)]) -> Request {
        let mut request = Request::post(path).header(header::HOST, "recipes.example.com");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::from("{}")).unwrap()
    }

    #[test]
    fn an_app_needs_a_plain_https_origin() {
        let app = HostApp::new("https://Recipes.Example.com:8443/", Router::new()).unwrap();
        assert_eq!(app.host(), "recipes.example.com");
        assert_eq!(app.origin(), "https://recipes.example.com:8443");
        let local = HostApp::new("http://Fond.localhost:8099", Router::new()).unwrap();
        assert_eq!(local.host(), "fond.localhost");
        assert_eq!(local.origin(), "http://fond.localhost:8099");
        assert!(HostApp::new("http://localhost", Router::new()).is_ok());
        for bad in [
            "http://recipes.example.com",
            "http://localhost.example.com",
            "https://recipes.example.com/app",
            "https://user:pw@recipes.example.com",
            "https://recipes.example.com?x=1",
            "recipes.example.com",
            "",
        ] {
            assert!(HostApp::new(bad, Router::new()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_app_host_reaches_only_the_app() {
        for host in [
            "recipes.example.com",
            "recipes.example.com:8080",
            "RECIPES.EXAMPLE.COM.",
        ] {
            assert_eq!(
                send(get_from(host, "/")).await,
                (StatusCode::OK, "app".to_owned()),
                "{host}"
            );
            let (status, body) = send(get_from(host, "/api/health")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{host}");
            assert_eq!(body, r#"{"error":"Not found"}"#);
            let (status, body) = send(get_from(host, "/anything")).await;
            assert_eq!(
                (status, body.as_str()),
                (StatusCode::NOT_FOUND, "Not found")
            );
        }
    }

    #[tokio::test]
    async fn other_hosts_never_reach_the_app() {
        for host in ["omni.boris", "10.10.1.100:8080", "example.com"] {
            assert_eq!(send(get_from(host, "/")).await.1, "omni", "{host}");
            assert_eq!(send(get_from(host, "/boom")).await.1, "omni spa");
            let (_, body) = send(
                Request::post("/api/notes")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(body, "omni spa", "{host}");
        }
    }

    #[tokio::test]
    async fn a_panic_in_the_app_is_an_opaque_500() {
        let (status, body) = send(get_from("recipes.example.com", "/boom")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
    }

    #[tokio::test]
    async fn mutations_need_the_app_origin_and_json() {
        let json = ("content-type", "application/json; charset=utf-8");
        let ok = send(post_to("/api/notes", &[("origin", ORIGIN), json])).await;
        assert_eq!(ok, (StatusCode::OK, "saved".to_owned()));
        let fetch = ("sec-fetch-site", "same-origin");
        let ok = send(post_to("/api/notes", &[("origin", ORIGIN), json, fetch])).await;
        assert_eq!(ok.0, StatusCode::OK);

        let refused: [&[(&str, &str)]; 7] = [
            &[json],
            &[("origin", "https://evil.example"), json],
            &[("origin", "http://recipes.example.com"), json],
            &[("origin", "https://recipes.example.com:8443"), json],
            &[("origin", "null"), json],
            &[("origin", ORIGIN), json, ("sec-fetch-site", "cross-site")],
            &[("origin", ORIGIN), json, ("sec-fetch-site", "same-site")],
        ];
        for headers in refused {
            let (status, body) = send(post_to("/api/notes", headers)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
            assert_eq!(body, r#"{"error":"Cross-origin requests are not allowed"}"#);
        }
        for headers in [
            &[("origin", ORIGIN)][..],
            &[("origin", ORIGIN), ("content-type", "text/plain")],
            &[("origin", ORIGIN), ("content-type", "multipart/form-data")],
            &[("origin", ORIGIN), ("x-fond-upload", "1")],
        ] {
            let (status, body) = send(post_to("/api/notes", headers)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
            assert_eq!(body, r#"{"error":"Expected application/json"}"#);
        }
    }

    #[tokio::test]
    async fn header_paths_take_the_custom_header_instead_of_json() {
        let upload = ("x-fond-upload", "1");
        let image = ("content-type", "image/jpeg");
        let ok = send(post_to("/api/upload", &[("origin", ORIGIN), upload, image])).await;
        assert_eq!(ok, (StatusCode::OK, "stored".to_owned()));

        let (status, body) = send(post_to("/api/upload", &[("origin", ORIGIN), image])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, r#"{"error":"Missing the upload header"}"#);
        // JSON does not stand in for the header there, and the origin still counts.
        let json = ("content-type", "application/json");
        let (status, _) = send(post_to("/api/upload", &[("origin", ORIGIN), json])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(post_to("/api/upload", &[upload, image])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let foreign = ("origin", "https://evil.example");
        let (status, _) = send(post_to("/api/upload", &[foreign, upload, image])).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn reads_pass_the_guard_untouched() {
        let guard = MutationGuard::new(ORIGIN);
        for method in [Method::GET, Method::HEAD] {
            let request = Request::builder()
                .method(method)
                .uri("/api/sync")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap();
            assert_eq!(guard.check(&request), Ok(()));
        }
        let options = Request::builder()
            .method(Method::OPTIONS)
            .uri("/api/upload")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert!(guard.check(&options).is_err());
    }
}
