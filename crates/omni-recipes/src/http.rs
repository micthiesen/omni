//! Fond's router: the app's API, its images and the PWA's static files. It
//! is registered only as a host app (`Subsystem::host_apps`), never merged
//! into Omni's router, so it answers on the recipes host and nowhere else.
//!
//! There is no catch-all: the four client routes return `index.html` and any
//! other path is the host app's 404.

use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_SECURITY_POLICY, CONTENT_TYPE, VARY,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::sse::Sse;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt as _;
use omni_api::fond::{
    AppendCookEventsRequest, CookActor, CookSession, Hello, InboxList, InboxUploadResponse,
    LibraryChanged, OpsRequest, OpsResponse, Original, SessionChanged, StartCookRequest,
    SyncResponse, paths, sse as frames,
};
use omni_core::clock::SharedClock;
use omni_server_kit::host_app::{HostApp, InvalidAppOrigin, MutationGuard, guard_mutations};
use omni_server_kit::{ApiError, JsonBodyLimit, sse};
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;

use crate::cook::{Append, Start};
use crate::db::{Actor, current_seq};
use crate::error::RecipesError;
use crate::files::Rendition;
use crate::hub::HubEvent;
use crate::inbox::MAX_PHOTO_BYTES;
use crate::service::Recipes;

/// The policy of a wasm PWA that loads only its own files.
pub const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data:; connect-src 'self'; worker-src 'self'; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const JSON_LIMIT: usize = 1024 * 1024;
const PING_EVERY: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct App {
    recipes: Recipes,
    dist: Arc<PathBuf>,
    clock: SharedClock,
    /// The running build, as `hello` and sync report it.
    build: Option<Arc<str>>,
}

fn api_error(error: RecipesError) -> ApiError {
    let status = match error.code() {
        "not_found" => StatusCode::NOT_FOUND,
        "too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "unsupported_media" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "merged" | "revision_conflict" | "session_closed" => StatusCode::CONFLICT,
        _ if error.is_rejection() => StatusCode::BAD_REQUEST,
        _ => return ApiError::internal(error.tool_text()),
    };
    ApiError::new(status, error.tool_text())
}

type Api<T> = Result<axum::Json<T>, ApiError>;

/// The env var the Docker image sets to its revision.
const REVISION_ENV: &str = "OMNI_IMAGE_REVISION";

/// The running build: the image revision when the image carries one, else a
/// fingerprint of the executable, so a local rebuild also reads as new. It is
/// the same value Omni's `/api/snapshot` reports as `build.server`; the app
/// checks for a new shell when it changes.
pub fn running_build() -> Option<String> {
    let revision = std::env::var(REVISION_ENV).ok();
    if let Some(revision) = revision.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        return Some(revision.chars().take(12).collect());
    }
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut hash = omni_core::digest::sha256_hex(
        format!("{}\n{}\n{modified}", exe.display(), meta.len()).as_bytes(),
    );
    hash.truncate(12);
    Some(hash)
}

#[derive(Deserialize)]
struct SyncQuery {
    since: Option<u64>,
}

async fn sync(State(app): State<App>, Query(query): Query<SyncQuery>) -> Api<SyncResponse> {
    let build = app.build.as_deref().map(str::to_owned);
    app.recipes
        .sync(query.since, build)
        .await
        .map(axum::Json)
        .map_err(api_error)
}

async fn events(State(app): State<App>) -> Result<Response, ApiError> {
    // Subscribe before reading the sequence number, so no change falls between.
    let receiver = app.recipes.hub().subscribe();
    let seq = app.recipes.query(current_seq).await.map_err(api_error)?;
    let hello = Hello {
        seq,
        build: app.build.as_deref().map(str::to_owned),
    };
    let ids = Arc::new(AtomicU64::new(1));
    let frame = move |name: &str, data: String| {
        sse::event(name, ids.fetch_add(1, Ordering::Relaxed), &data)
    };
    let first = frame(
        frames::HELLO,
        serde_json::to_string(&hello).map_err(ApiError::internal)?,
    );
    // A receiver that fell behind ends the stream: the app reconnects and
    // starts again from `hello`.
    let changes = BroadcastStream::new(receiver)
        .take_while(|event| std::future::ready(event.is_ok()))
        .filter_map(move |event| {
            let frame = frame.clone();
            async move {
                match event.ok()? {
                    HubEvent::Library { seq } => Some(frame(
                        frames::LIBRARY,
                        serde_json::to_string(&LibraryChanged { seq }).ok()?,
                    )),
                    HubEvent::Session { id, seq } => Some(frame(
                        frames::SESSION,
                        serde_json::to_string(&SessionChanged { id, seq }).ok()?,
                    )),
                }
            }
        });
    let stream = futures::stream::once(std::future::ready(first)).chain(changes);
    Ok((
        sse::HEADERS,
        Sse::new(sse::with_ping(stream, PING_EVERY, app.clock.clone())),
    )
        .into_response())
}

async fn ops(
    State(app): State<App>,
    JsonBodyLimit(request): JsonBodyLimit<JSON_LIMIT, OpsRequest>,
) -> Api<OpsResponse> {
    if request.ops.len() > 500 {
        return Err(ApiError::bad_request("at most 500 ops per request"));
    }
    app.recipes
        .apply_ops(request.ops)
        .await
        .map(axum::Json)
        .map_err(api_error)
}

async fn cook_start(
    State(app): State<App>,
    JsonBodyLimit(request): JsonBodyLimit<JSON_LIMIT, StartCookRequest>,
) -> Api<CookSession> {
    app.recipes
        .start_session(Start {
            id: Some(request.id),
            recipe_id: request.recipe_id,
            revision: Some(request.revision),
            scale: Some(request.scale),
            servings: None,
        })
        .await
        .map(axum::Json)
        .map_err(api_error)
}

async fn cook_get(State(app): State<App>, Path(id): Path<String>) -> Api<CookSession> {
    app.recipes
        .session(&id)
        .await
        .map(axum::Json)
        .map_err(api_error)
}

async fn cook_events(
    State(app): State<App>,
    Path(id): Path<String>,
    JsonBodyLimit(request): JsonBodyLimit<JSON_LIMIT, AppendCookEventsRequest>,
) -> Api<CookSession> {
    // The app starts sessions offline: events can reach the server before the
    // start they belong to. "Too early" keeps them in its outbox; any other
    // 4xx would make it drop them.
    if crate::ids::is_id(&id, crate::ids::COOK)
        && matches!(app.recipes.session(&id).await, Err(ref error) if error.code() == "not_found")
    {
        return Err(ApiError::new(
            StatusCode::TOO_EARLY,
            format!("[not_found] no cook session has id {id:?} yet"),
        ));
    }
    if request.events.len() > 1000 {
        return Err(ApiError::bad_request("at most 1000 events per request"));
    }
    let mut events = Vec::with_capacity(request.events.len());
    for event in request.events {
        let id = &event.client_event_id;
        if id.is_empty() || id.len() > 100 || id.chars().any(char::is_control) {
            return Err(ApiError::bad_request(
                "clientEventId must be 1 to 100 printable characters",
            ));
        }
        events.push(Append {
            // Whatever the body says, what arrives here came from the app.
            actor: CookActor::App,
            client_event_id: event.client_event_id,
            at: event.at,
            kind: event.kind,
        });
    }
    app.recipes
        .append_events(&id, events, false, Actor::app())
        .await
        .map(axum::Json)
        .map_err(api_error)
}

#[derive(Deserialize)]
struct PhotoQuery {
    id: Option<String>,
}

async fn cook_photo(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<PhotoQuery>,
    body: Bytes,
) -> Api<CookSession> {
    if query
        .id
        .as_deref()
        .is_some_and(|upload| !crate::ids::is_client_token(upload))
    {
        return Err(ApiError::bad_request(
            "id must be 1 to 64 letters, digits, _ or -",
        ));
    }
    app.recipes
        .cook_photo(&id, query.id, body.to_vec())
        .await
        .map(axum::Json)
        .map_err(api_error)
}

async fn inbox_list(State(app): State<App>) -> Api<InboxList> {
    app.recipes
        .inbox_for_app()
        .await
        .map(axum::Json)
        .map_err(api_error)
}

#[derive(Deserialize)]
struct UploadQuery {
    item: String,
    index: u32,
    count: u32,
    title: Option<String>,
}

async fn inbox_upload(
    State(app): State<App>,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Api<InboxUploadResponse> {
    app.recipes
        .upload_photo(
            &query.item,
            query.index,
            query.count,
            query.title,
            body.to_vec(),
        )
        .await
        .map(|upload| {
            axum::Json(InboxUploadResponse {
                item_id: upload.item_id,
                received: upload.received,
                complete: upload.complete,
            })
        })
        .map_err(api_error)
}

async fn original(State(app): State<App>, Path(id): Path<String>) -> Api<Original> {
    app.recipes
        .original(&id)
        .await
        .map(axum::Json)
        .map_err(api_error)
}

fn not_found() -> Response {
    let mut response = Response::new(Body::from("Not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    response
}

fn jpeg(bytes: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// `GET /img/:digest/:variant.jpg`: a rendition of an image some recipe,
/// inbox item or cook photo refers to. Any other digest does not exist.
async fn image(
    State(app): State<App>,
    Path((digest, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let Some(rendition) = file.strip_suffix(".jpg").and_then(Rendition::parse) else {
        return Ok(not_found());
    };
    let wanted = digest.clone();
    let referenced = app
        .recipes
        .query(move |conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM recipes_image WHERE sha256 = ?1)
                     OR EXISTS (SELECT 1 FROM recipes_inbox_file WHERE sha256 = ?1)",
                rusqlite::params![wanted],
                |row| row.get::<_, bool>(0),
            )
            .map_err(crate::error::sql)
        })
        .await
        .map_err(api_error)?;
    if !referenced {
        return Ok(not_found());
    }
    match app
        .recipes
        .files()
        .read(&digest, rendition)
        .await
        .map_err(api_error)?
    {
        Some(bytes) => Ok(jpeg(bytes)),
        None => Ok(not_found()),
    }
}

/// `GET /src/:itemId/:n.jpg`: photo `n` of an inbox item, at reading size.
async fn source_photo(
    State(app): State<App>,
    Path((item_id, file)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let Some(index) = file
        .strip_suffix(".jpg")
        .and_then(|index| index.parse::<u32>().ok())
    else {
        return Ok(not_found());
    };
    let digest = app
        .recipes
        .query(move |conn| {
            Ok(crate::inbox::files(conn, &item_id)?
                .into_iter()
                .find(|file| file.idx == index)
                .map(|file| file.sha256))
        })
        .await
        .map_err(api_error)?;
    let Some(digest) = digest else {
        return Ok(not_found());
    };
    match app
        .recipes
        .files()
        .read(&digest, Rendition::Full)
        .await
        .map_err(api_error)?
    {
        Some(bytes) => Ok(jpeg(bytes)),
        None => Ok(not_found()),
    }
}

fn content_type(path: &FsPath) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serves `dist/relative`, preferring a precompressed `.br` or `.gz` sibling
/// the client accepts. `None` when the file does not exist.
async fn static_file(
    dist: &FsPath,
    relative: &FsPath,
    headers: &HeaderMap,
    cache: &'static str,
) -> Option<Response> {
    // Only plain names below the app directory: no `..`, no absolute path.
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let path = dist.join(relative);
    let accepts = |coding: &str| {
        headers
            .get(ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|part| part.split(';').next().map(str::trim) == Some(coding))
            })
    };
    let mut found: Option<(Vec<u8>, Option<&'static str>)> = None;
    for (coding, suffix) in [("br", "br"), ("gzip", "gz")] {
        if accepts(coding) {
            let mut name = path.clone().into_os_string();
            name.push(".");
            name.push(suffix);
            if let Ok(bytes) = tokio::fs::read(PathBuf::from(name)).await {
                found = Some((bytes, Some(coding)));
                break;
            }
        }
    }
    let (bytes, coding) = match found {
        Some(found) => found,
        None => (tokio::fs::read(&path).await.ok()?, None),
    };
    let mut response = Response::new(Body::from(bytes));
    let out = response.headers_mut();
    out.insert(CONTENT_TYPE, HeaderValue::from_static(content_type(&path)));
    out.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    out.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(coding) = coding {
        out.insert(CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    Some(response)
}

/// The app shell, for each of the app's client routes.
async fn index(State(app): State<App>, headers: HeaderMap) -> Response {
    match static_file(&app.dist, FsPath::new("index.html"), &headers, "no-cache").await {
        Some(page) => page,
        None => {
            let mut response = Response::new(Body::from(
                "Fond is not built on this server yet: the app's files are missing. The recipe tools and the API work.",
            ));
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            let headers = response.headers_mut();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=UTF-8"),
            );
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
    }
}

async fn root_file(State(app): State<App>, request: Request) -> Response {
    let name = request.uri().path().trim_start_matches('/').to_owned();
    static_file(&app.dist, FsPath::new(&name), request.headers(), "no-cache")
        .await
        .unwrap_or_else(not_found)
}

async fn asset(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Response {
    static_file(
        &app.dist,
        &FsPath::new("assets").join(path),
        &headers,
        IMMUTABLE,
    )
    .await
    .unwrap_or_else(not_found)
}

async fn icon(State(app): State<App>, Path(path): Path<String>, headers: HeaderMap) -> Response {
    static_file(
        &app.dist,
        &FsPath::new("icons").join(path),
        &headers,
        "public, max-age=604800",
    )
    .await
    .unwrap_or_else(not_found)
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// The paths that take the upload header instead of a JSON body.
pub fn is_upload_path(path: &str) -> bool {
    path == paths::INBOX || (path.starts_with("/api/cook/") && path.ends_with("/photo"))
}

/// Fond's routes with state applied, for the app at `origin`.
pub fn router(recipes: Recipes, origin: &str, dist: PathBuf, build: Option<String>) -> Router {
    let clock = recipes.clock.clone();
    let app = App {
        recipes,
        dist: Arc::new(dist),
        clock,
        build: build.map(Arc::from),
    };
    let guard = MutationGuard::new(origin).with_header_paths(
        HeaderName::from_static(paths::UPLOAD_HEADER),
        is_upload_path,
    );
    let uploads = Router::new()
        .route(paths::INBOX, post(inbox_upload))
        .route("/api/cook/{id}/photo", post(cook_photo))
        .layer(DefaultBodyLimit::max(MAX_PHOTO_BYTES));
    Router::new()
        .route(paths::SYNC, get(sync))
        .route(paths::EVENTS, get(events))
        .route(paths::OPS, post(ops))
        .route(paths::COOK, post(cook_start))
        .route("/api/cook/{id}", get(cook_get))
        .route("/api/cook/{id}/events", post(cook_events))
        .route(paths::INBOX, get(inbox_list))
        .route("/api/original/{id}", get(original))
        .merge(uploads)
        .route("/img/{digest}/{file}", get(image))
        .route("/src/{item}/{file}", get(source_photo))
        .route("/", get(index))
        .route("/r/{id}", get(index))
        .route("/cook/{id}", get(index))
        .route("/inbox", get(index))
        .route("/sw.js", get(root_file))
        .route("/manifest.webmanifest", get(root_file))
        .route("/assets/{*path}", get(asset))
        .route("/icons/{*path}", get(icon))
        .layer(axum::middleware::from_fn_with_state(guard, guard_mutations))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(app)
}

/// The host app serving Fond at `origin`.
pub fn host_app(
    recipes: Recipes,
    origin: &str,
    dist: PathBuf,
) -> Result<HostApp, InvalidAppOrigin> {
    HostApp::new(origin, router(recipes, origin, dist, running_build()))
}
