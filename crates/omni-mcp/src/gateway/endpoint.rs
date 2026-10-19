//! `ALL /gateway/mcp`.
//!
//! Every request needs a gateway bearer token (a client token or an OAuth
//! access token, [`super::access`]); anything else is a 401 that names the
//! protected-resource metadata when downstream OAuth is configured. POSTs
//! are served by the shared protocol layer in gateway mode. A GET that
//! accepts `text/event-stream` opens a notification stream that carries
//! `notifications/tools/list_changed` and `notifications/resources/list_changed`
//! whenever the served catalog changes (the stream is stateless and shared
//! by every client); it ends when Omni shuts down.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_LENGTH, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use futures::StreamExt as _;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::access::DownstreamAccess;
use super::service::{CatalogChange, Gateway};
use crate::json::json_response;
use crate::rpc::{MAX_REQUEST_BYTES, McpProtocol};

pub const PATH: &str = omni_api::gateway::GATEWAY_MCP_PATH;
/// Concurrent notification streams.
const MAX_STREAMS: usize = 32;

#[derive(Clone)]
struct EndpointState {
    protocol: McpProtocol,
    access: DownstreamAccess,
    gateway: Gateway,
    /// `resource_metadata` for 401 challenges, when OAuth is offered.
    resource_metadata: Option<Arc<str>>,
    streams: Arc<Semaphore>,
    shutdown: CancellationToken,
}

pub fn router(
    protocol: McpProtocol,
    access: DownstreamAccess,
    gateway: Gateway,
    resource_metadata: Option<String>,
    shutdown: CancellationToken,
) -> Router {
    Router::new()
        .route(PATH, any(handle))
        // Everything under /gateway/ is public on the proxy host, so unknown
        // paths are a 404 here rather than the UI's SPA fallback.
        .route("/gateway/{*rest}", any(not_found))
        .with_state(EndpointState {
            protocol,
            access,
            gateway,
            resource_metadata: resource_metadata.map(Arc::from),
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
        })
}

async fn not_found() -> Response {
    json_response(StatusCode::NOT_FOUND, &json!({"error": "Not found"}))
}

fn unauthorized(resource_metadata: Option<&str>) -> Response {
    let mut response = json_response(StatusCode::UNAUTHORIZED, &json!({"error": "Unauthorized"}));
    let challenge = match resource_metadata {
        Some(url) => format!(r#"Bearer resource_metadata="{url}", scope="omni""#),
        None => "Bearer".to_owned(),
    };
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        headers.insert(WWW_AUTHENTICATE, value);
    }
    response
}

fn wants_stream(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"))
}

async fn handle(
    State(state): State<EndpointState>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let authorization = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    let Some(principal) = state.access.authenticate(authorization).await else {
        return unauthorized(state.resource_metadata.as_deref());
    };
    let mut response = if method == Method::GET && wants_stream(&headers) {
        notification_stream(&state)
    } else {
        let declared = headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let bytes = match declared {
            Some(length) if length > MAX_REQUEST_BYTES as u64 => None,
            _ => axum::body::to_bytes(body, MAX_REQUEST_BYTES).await.ok(),
        };
        match bytes {
            Some(bytes) => {
                state
                    .protocol
                    .serve_gateway(&principal, &method, &headers, bytes)
                    .await
            }
            None => json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "Payload too large"}, "id": null}),
            ),
        }
    };
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn notification(change: CatalogChange) -> Event {
    let method = match change {
        CatalogChange::Tools => "notifications/tools/list_changed",
        CatalogChange::Resources => "notifications/resources/list_changed",
    };
    Event::default()
        .event("message")
        .data(json!({"jsonrpc": "2.0", "method": method}).to_string())
}

fn notification_stream(state: &EndpointState) -> Response {
    let Ok(permit) = state.streams.clone().try_acquire_owned() else {
        return json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &json!({"error": "Too many notification streams"}),
        );
    };
    let changes = tokio_stream_from(state.gateway.changes());
    let shutdown = state.shutdown.clone().cancelled_owned();
    let stream = changes
        .map(move |change| {
            let _held = &permit;
            Ok::<Event, Infallible>(notification(change))
        })
        .take_until(shutdown);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(25)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// The catalog changes as a stream. A lagging receiver missed changes it
/// cannot name, so it reports a tool change (clients then list again).
fn tokio_stream_from(
    receiver: tokio::sync::broadcast::Receiver<CatalogChange>,
) -> impl futures::Stream<Item = CatalogChange> + Send + 'static {
    futures::stream::unfold(receiver, |mut receiver| async move {
        match receiver.recv().await {
            Ok(change) => Some((change, receiver)),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                Some((CatalogChange::Tools, receiver))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
}
