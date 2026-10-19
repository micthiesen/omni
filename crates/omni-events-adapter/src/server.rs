//! HTTP front door: `/health`, legacy passthrough and the modern MCP surface.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use url::Url;

use crate::auth::{AuthenticatedOwner, authenticate_oauth};
use crate::config::{AdapterOptions, PROTOCOL_VERSION, USER_AGENT};
use crate::continuations::{
    CallRequest, ContinuationError, Continuations, DEFAULT_MAX_ACTIVE, DEFAULT_TTL,
};
use crate::legacy::{
    CLOSE_TIMEOUT, DEFAULT_REQUEST_TIMEOUT, ElicitationSupport, HttpConnector, LegacyConnector,
};
use crate::rpc::{
    CLIENT_CAPABILITIES_KEY, PROTOCOL_VERSION_KEY, RpcRequest, complete, error_body,
    fill_cache_fields, result_body,
};
use crate::upstreams::Upstreams;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_MODERN_BODY_BYTES: usize = 262_144;
const MAX_OMNI_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const PROXY_TIMEOUT: Duration = Duration::from_secs(3600);
const OMNI_TIMEOUT: Duration = Duration::from_secs(30);
const HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const LEGACY_METHODS: [&str; 6] = [
    "tools/list",
    "resources/list",
    "resources/read",
    "resources/templates/list",
    "prompts/list",
    "prompts/get",
];
const EVENT_METHODS: [&str; 3] = ["events/list", "events/subscribe", "events/unsubscribe"];

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("could not build the HTTP client: {0}")]
    Http(#[from] reqwest::Error),
}

/// Failures answered with `502 {"error":"Upstream unavailable"}`.
#[derive(Debug, thiserror::Error)]
enum Unavailable {
    #[error("invalid request body")]
    Body,
    #[error("Executor proxy unavailable: {0}")]
    Proxy(reqwest::Error),
    #[error("Omni Events unavailable: {0}")]
    Omni(String),
}

#[derive(Clone)]
struct AppState {
    options: Arc<AdapterOptions>,
    http: reqwest::Client,
    proxy: reqwest::Client,
    continuations: Continuations,
    connector: Arc<dyn LegacyConnector>,
    upstreams: Upstreams,
}

/// Configures the adapter; the defaults are production's.
pub struct AdapterBuilder {
    options: AdapterOptions,
    connector: Option<Arc<dyn LegacyConnector>>,
    ttl: Duration,
    max_active: usize,
}

/// A built adapter: its router and continuation manager.
pub struct Adapter {
    router: Router,
    continuations: Continuations,
}

impl AdapterBuilder {
    pub fn new(options: AdapterOptions) -> Self {
        Self {
            options,
            connector: None,
            ttl: DEFAULT_TTL,
            max_active: DEFAULT_MAX_ACTIVE,
        }
    }

    /// Replaces the Executor legacy MCP connector (tests).
    pub fn connector(mut self, connector: Arc<dyn LegacyConnector>) -> Self {
        self.connector = Some(connector);
        self
    }

    pub fn continuation_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    pub fn max_active(mut self, max_active: usize) -> Self {
        self.max_active = max_active;
        self
    }

    pub fn build(self) -> Result<Adapter, BuildError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        // Bytes pass through untouched, with their content-encoding.
        let proxy = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()?;
        let connector = self.connector.unwrap_or_else(|| {
            Arc::new(HttpConnector::new(
                http.clone(),
                self.options.executor_base_url.clone(),
            ))
        });
        let continuations = Continuations::new(Arc::clone(&connector), self.ttl, self.max_active);
        let upstreams = Upstreams::new(self.options.event_upstreams.clone(), http.clone());
        let state = AppState {
            options: Arc::new(self.options),
            http,
            proxy,
            continuations: continuations.clone(),
            connector,
            upstreams,
        };
        let router = Router::new().fallback(handle).with_state(state);
        Ok(Adapter {
            router,
            continuations,
        })
    }
}

impl Adapter {
    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn continuations(&self) -> &Continuations {
        &self.continuations
    }

    /// Serves until `shutdown` resolves.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
    }
}

async fn handle(State(state): State<AppState>, request: Request) -> Response {
    match route(&state, request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "adapter request failed");
            json_response(
                StatusCode::BAD_GATEWAY,
                &json!({"error": "Upstream unavailable"}),
                &[],
            )
        }
    }
}

fn json_response(status: StatusCode, value: &Value, extra: &[(&'static str, String)]) -> Response {
    let mut response = (status, value.to_string()).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    response
}

fn rpc_error(id: Value, code: i64, message: &str, status: StatusCode) -> Response {
    json_response(status, &error_body(id, code, message, None), &[])
}

/// Node's view of a repeated request header: values joined with ", ".
fn joined_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Request headers Node keeps only the first value of (`IncomingMessage.headers`).
const SINGLE_VALUE_HEADERS: [&str; 17] = [
    "age",
    "authorization",
    "content-length",
    "content-type",
    "etag",
    "expires",
    "from",
    "host",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "referer",
    "retry-after",
    "user-agent",
];

/// A repeated request header folded the way Node does: the first value of a
/// single-value header, cookies joined with "; ", others with ", ".
fn node_header_value(headers: &HeaderMap, name: &HeaderName) -> Option<HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if SINGLE_VALUE_HEADERS.contains(&name.as_str()) {
        return Some(first.clone());
    }
    let separator: &[u8] = if name == header::COOKIE { b"; " } else { b", " };
    let mut joined = first.as_bytes().to_vec();
    for value in values {
        joined.extend_from_slice(separator);
        joined.extend_from_slice(value.as_bytes());
    }
    HeaderValue::from_bytes(&joined).ok()
}

fn query_value(query: Option<&str>, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

async fn route(state: &AppState, request: Request) -> Result<Response, Unavailable> {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path();
    if path == "/health" && parts.method == Method::GET {
        return Ok(json_response(StatusCode::OK, &json!({"status": "ok"}), &[]));
    }
    if path != "/mcp" {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            &json!({"error": "Not found"}),
            &[],
        ));
    }
    let raw = if parts.method == Method::POST {
        Some(
            axum::body::to_bytes(body, MAX_BODY_BYTES)
                .await
                .map_err(|_| Unavailable::Body)?,
        )
    } else {
        None
    };
    let message = raw.as_deref().and_then(RpcRequest::parse);
    let metadata = message.as_ref().and_then(RpcRequest::meta);
    let header_version = joined_header(&parts.headers, "mcp-protocol-version");
    let metadata_version = metadata.and_then(|m| m.get(PROTOCOL_VERSION_KEY));
    let version = header_version
        .clone()
        .map(Value::String)
        .or_else(|| metadata_version.cloned());
    let modern = match &version {
        Some(Value::String(v)) => v == PROTOCOL_VERSION || v.starts_with("2026-"),
        _ => false,
    } || message
        .as_ref()
        .is_some_and(|m| m.method == "server/discover");
    let query = parts.uri.query();
    if !modern {
        return proxy_legacy(state, &parts.method, &parts.headers, query, raw).await;
    }
    let id = message.as_ref().map_or(Value::Null, |m| m.id.clone());
    if raw
        .as_ref()
        .is_some_and(|raw| raw.len() > MAX_MODERN_BODY_BYTES)
    {
        return Ok(rpc_error(
            id,
            -32600,
            "Request too large",
            StatusCode::PAYLOAD_TOO_LARGE,
        ));
    }
    let (Some(message), Some(raw)) = (message.as_ref(), raw) else {
        return Ok(rpc_error(
            id,
            -32600,
            "Invalid request",
            StatusCode::BAD_REQUEST,
        ));
    };
    if let (Some(header), Some(meta)) = (&header_version, metadata_version)
        && meta.as_str() != Some(header.as_str())
    {
        return Ok(rpc_error(
            id,
            -32020,
            "Protocol version header mismatch",
            StatusCode::BAD_REQUEST,
        ));
    }
    if let Some(version) = version.filter(|v| v.as_str() != Some(PROTOCOL_VERSION)) {
        let body = error_body(
            id,
            -32022,
            "Unsupported protocol version",
            Some(json!({"requested": version, "supported": [PROTOCOL_VERSION]})),
        );
        return Ok(json_response(StatusCode::BAD_REQUEST, &body, &[]));
    }
    let authorization = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let identity = match authenticate_oauth(
        &state.http,
        authorization,
        &state.options,
        jiff::Timestamp::now(),
    )
    .await
    {
        Err(_) => {
            return Ok(rpc_error(
                id,
                -32603,
                "Executor authentication unavailable",
                StatusCode::SERVICE_UNAVAILABLE,
            ));
        }
        Ok(None) => return Ok(unauthorized(id, &state.options.public_mcp_origin)),
        Ok(Some(identity)) => identity,
    };
    if message.method.starts_with("events/") {
        if !EVENT_METHODS.contains(&message.method.as_str()) {
            return Ok(rpc_error(id, -32601, "Method not found", StatusCode::OK));
        }
        if !state.upstreams.is_empty() {
            if message.method == "events/list" {
                return list_events(state, raw, &identity).await;
            }
            if let Some(response) = upstream_subscription(state, message, id.clone()).await {
                return Ok(response);
            }
        }
        return forward_event(state, &message.method, raw, &identity).await;
    }
    if message.method == "server/discover" {
        let mut body = Map::new();
        body.insert("supportedVersions".into(), json!([PROTOCOL_VERSION]));
        body.insert(
            "capabilities".into(),
            json!({"tools": {}, "resources": {}, "events": {}}),
        );
        return Ok(json_response(StatusCode::OK, &result_body(id, body), &[]));
    }
    let params = message.params.clone().unwrap_or_default();
    let mode = query_value(query, "elicitation_mode");
    if message.method == "tools/call" {
        let support = elicitation_support(metadata);
        let outcome = state
            .continuations
            .call(CallRequest {
                owner: identity.owner,
                authorization: identity.authorization,
                mode,
                params,
                support,
            })
            .await;
        return Ok(match outcome {
            Ok(body) => json_response(StatusCode::OK, &result_body(id, body), &[]),
            Err(ContinuationError::Upstream) => {
                rpc_error(id, -32603, "Executor tool request failed", StatusCode::OK)
            }
            Err(ContinuationError::Capacity) => {
                rpc_error(id, -32000, "Tool capacity exceeded", StatusCode::OK)
            }
            Err(ContinuationError::Invalid) => {
                rpc_error(id, -32602, "Tool continuation unavailable", StatusCode::OK)
            }
        });
    }
    if !LEGACY_METHODS.contains(&message.method.as_str()) {
        return Ok(rpc_error(id, -32601, "Method not found", StatusCode::OK));
    }
    Ok(
        match call_legacy(state, identity.authorization, mode, &message.method, params).await {
            Some(body) => json_response(StatusCode::OK, &result_body(id, body), &[]),
            None => rpc_error(
                id,
                -32603,
                "Executor tool request failed",
                StatusCode::BAD_GATEWAY,
            ),
        },
    )
}

/// Which elicitation modes the modern caller advertised in request metadata.
fn elicitation_support(metadata: Option<&Map<String, Value>>) -> ElicitationSupport {
    let advertised = metadata
        .and_then(|m| m.get(CLIENT_CAPABILITIES_KEY))
        .and_then(Value::as_object)
        .and_then(|c| c.get("elicitation"))
        .and_then(Value::as_object);
    match advertised {
        None => ElicitationSupport::default(),
        Some(modes) => ElicitationSupport {
            form: modes.contains_key("form") || !modes.contains_key("url"),
            url: modes.contains_key("url"),
        },
    }
}

fn unauthorized(id: Value, public_mcp_origin: &Url) -> Response {
    let metadata = public_mcp_origin
        .join("/.well-known/oauth-protected-resource")
        .map(String::from)
        .unwrap_or_default();
    json_response(
        StatusCode::UNAUTHORIZED,
        &error_body(id, -32001, "Unauthorized", None),
        &[
            (
                "www-authenticate",
                format!("Bearer resource_metadata=\"{metadata}\""),
            ),
            ("access-control-allow-origin", "*".to_owned()),
            (
                "access-control-expose-headers",
                "WWW-Authenticate".to_owned(),
            ),
        ],
    )
}

/// Drops the modern hop's per-request `io.modelcontextprotocol/*` metadata
/// (protocol version, client info and capabilities): it describes the caller's
/// 2026 session, and a legacy server that validates it rejects the request.
fn legacy_params(mut params: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::Object(meta)) = params.get_mut("_meta") {
        meta.retain(|key, _| !key.starts_with("io.modelcontextprotocol/"));
        if meta.is_empty() {
            params.remove("_meta");
        }
    }
    params
}

/// One scoped legacy session per request; `None` on any failure.
async fn call_legacy(
    state: &AppState,
    authorization: String,
    mode: Option<String>,
    method: &str,
    params: Map<String, Value>,
) -> Option<Map<String, Value>> {
    let session = match state
        .connector
        .connect(authorization, mode, ElicitationSupport::default(), None)
        .await
    {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(%error, method, "Executor MCP connection failed");
            return None;
        }
    };
    let params = Value::Object(legacy_params(params));
    let outcome = session
        .request(method, params, DEFAULT_REQUEST_TIMEOUT)
        .await;
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, session.close()).await;
    match outcome {
        Ok(Value::Object(mut body)) => {
            fill_cache_fields(method, &mut body);
            Some(body)
        }
        // A result that is not an object is rejected.
        Ok(_) => {
            tracing::warn!(method, "Executor MCP returned a non-object result");
            None
        }
        Err(error) => {
            tracing::warn!(%error, method, "Executor MCP request failed");
            None
        }
    }
}

/// Omni's events, then the upstreams' (except any name Omni already has).
async fn list_events(
    state: &AppState,
    raw: Bytes,
    identity: &AuthenticatedOwner,
) -> Result<Response, Unavailable> {
    let (status, mut rpc) = omni_event(state, "events/list", raw, identity).await?;
    if let Some(Value::Object(result)) = rpc.get_mut("result") {
        let catalog = state.upstreams.catalog().await;
        if let Some(Value::Array(events)) = result.get_mut("events") {
            let taken: Vec<String> = events
                .iter()
                .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect();
            events.extend(catalog.events_except(&taken));
        }
    }
    Ok(json_response(status, &Value::Object(rpc), &[]))
}

/// A subscribe or unsubscribe for an upstream's event, sent there with its token. None for Omni's events.
async fn upstream_subscription(
    state: &AppState,
    message: &RpcRequest,
    id: Value,
) -> Option<Response> {
    let params = message.params.clone().unwrap_or_default();
    let name = params.get("name").and_then(Value::as_str)?.to_owned();
    let arguments = match params.get("arguments") {
        Some(Value::Object(a)) => a.clone(),
        _ => Map::new(),
    };
    let catalog = state.upstreams.catalog().await;
    let route = match catalog.route(&name, &arguments)? {
        Ok(route) => route,
        Err(problem) => return Some(rpc_error(id, -32602, &problem, StatusCode::OK)),
    };
    let upstream = state.upstreams.get(&route.key)?;
    let mut forwarded = params;
    // The upstream speaks for itself; this request's protocol metadata stays here.
    forwarded.remove("_meta");
    forwarded.insert("arguments".into(), Value::Object(route.arguments));
    Some(
        match state
            .upstreams
            .call(upstream, &message.method, forwarded)
            .await
        {
            Ok(mut rpc) => {
                rpc.insert("id".into(), id);
                if let Some(Value::Object(result)) = rpc.get_mut("result") {
                    let wrapped = complete(std::mem::take(result));
                    *result = wrapped;
                }
                json_response(StatusCode::OK, &Value::Object(rpc), &[])
            }
            Err(error) => {
                tracing::warn!(upstream = %route.key, %error, method = %message.method, "upstream event request failed");
                rpc_error(
                    id,
                    -32603,
                    "That workspace's events are unavailable right now",
                    StatusCode::OK,
                )
            }
        },
    )
}

async fn forward_event(
    state: &AppState,
    method: &str,
    raw: Bytes,
    identity: &AuthenticatedOwner,
) -> Result<Response, Unavailable> {
    let (status, rpc) = omni_event(state, method, raw, identity).await?;
    Ok(json_response(status, &Value::Object(rpc), &[]))
}

/// An events/* call to Omni for the authenticated owner: its status and JSON-RPC response (result wrapped).
async fn omni_event(
    state: &AppState,
    method: &str,
    raw: Bytes,
    identity: &AuthenticatedOwner,
) -> Result<(StatusCode, Map<String, Value>), Unavailable> {
    let omni = |e: String| Unavailable::Omni(e);
    let url = state
        .options
        .omni_base_url
        .join("/mcp")
        .map_err(|e| omni(e.to_string()))?;
    let upstream = state
        .http
        .post(url)
        .bearer_auth(&state.options.omni_mcp_token)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL_VERSION)
        .header("mcp-method", method)
        .header("x-omni-events-owner", &identity.owner)
        .header("x-omni-events-authorization", &identity.authorization)
        .header(header::USER_AGENT, USER_AGENT)
        .body(raw)
        .timeout(OMNI_TIMEOUT)
        .send()
        .await
        .map_err(|e| omni(e.to_string()))?;
    let status = upstream.status();
    if status.is_redirection() {
        return Err(omni(format!("unexpected redirect {status}")));
    }
    let mut body = Vec::new();
    let mut stream = upstream.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| omni(e.to_string()))?;
        if body.len() + chunk.len() > MAX_OMNI_RESPONSE_BYTES {
            return Err(omni("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    let Ok(Value::Object(mut rpc)) = serde_json::from_slice::<Value>(&body) else {
        return Err(omni("Invalid Omni response".into()));
    };
    if let Some(Value::Object(result)) = rpc.get_mut("result") {
        let wrapped = complete(std::mem::take(result));
        *result = wrapped;
    }
    Ok((status, rpc))
}

async fn proxy_legacy(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    query: Option<&str>,
    raw: Option<Bytes>,
) -> Result<Response, Unavailable> {
    let mut target = state
        .options
        .executor_base_url
        .join("/mcp")
        .map_err(|_| Unavailable::Body)?;
    target.set_query(query.filter(|q| !q.is_empty()));
    let mut forwarded = HeaderMap::new();
    for name in headers.keys() {
        let key = name.as_str();
        if HOP_HEADERS.contains(&key) || key == "host" || key == "content-length" {
            continue;
        }
        if let Some(value) = node_header_value(headers, name) {
            forwarded.insert(name.clone(), value);
        }
    }
    forwarded.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    let mut request = state
        .proxy
        .request(method.clone(), target)
        .headers(forwarded)
        .timeout(PROXY_TIMEOUT);
    if let Some(raw) = raw {
        request = request.body(raw);
    }
    let upstream = request.send().await.map_err(Unavailable::Proxy)?;
    let mut response = Response::builder().status(upstream.status());
    if let Some(out) = response.headers_mut() {
        for (name, value) in upstream.headers() {
            let key = name.as_str();
            if !HOP_HEADERS.contains(&key) && key != "content-length" {
                out.append(name.clone(), value.clone());
            }
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|_| Unavailable::Body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elicitation_support_rules() {
        let meta = |caps: Value| {
            json!({ CLIENT_CAPABILITIES_KEY: caps })
                .as_object()
                .cloned()
                .unwrap()
        };
        assert_eq!(elicitation_support(None), ElicitationSupport::default());
        assert_eq!(
            elicitation_support(Some(&meta(json!({"elicitation": {}})))),
            ElicitationSupport {
                form: true,
                url: false
            }
        );
        assert_eq!(
            elicitation_support(Some(&meta(json!({"elicitation": {"url": {}}})))),
            ElicitationSupport {
                form: false,
                url: true
            }
        );
        assert_eq!(
            elicitation_support(Some(&meta(json!({"elicitation": {"form": {}, "url": {}}})))),
            ElicitationSupport {
                form: true,
                url: true
            }
        );
        assert_eq!(
            elicitation_support(Some(&meta(json!({"elicitation": true})))),
            ElicitationSupport::default()
        );
    }

    #[test]
    fn legacy_params_keep_only_legacy_metadata() {
        let params = json!({"cursor": "c", "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {},
            "progressToken": 1,
        }});
        assert_eq!(
            Value::Object(legacy_params(params.as_object().cloned().unwrap())),
            json!({"cursor": "c", "_meta": {"progressToken": 1}})
        );
        let only_modern = json!({"_meta": {"io.modelcontextprotocol/protocolVersion": "x"}});
        assert_eq!(
            Value::Object(legacy_params(only_modern.as_object().cloned().unwrap())),
            json!({})
        );
    }

    #[test]
    fn repeated_request_headers_follow_node() {
        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("a=1"));
        headers.append(header::COOKIE, HeaderValue::from_static("b=2"));
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer one"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer two"),
        );
        headers.append(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.append(header::ACCEPT, HeaderValue::from_static("application/json"));
        let value = |name| node_header_value(&headers, &name).unwrap();
        assert_eq!(value(header::COOKIE), "a=1; b=2");
        assert_eq!(value(header::AUTHORIZATION), "Bearer one");
        assert_eq!(value(header::ACCEPT), "text/event-stream, application/json");
    }

    #[test]
    fn query_value_reads_the_first_decoded_value() {
        assert_eq!(
            query_value(
                Some("a=1&elicitation_mode=na%74ive&elicitation_mode=x"),
                "elicitation_mode"
            ),
            Some("native".into())
        );
        assert_eq!(query_value(None, "elicitation_mode"), None);
    }
}
