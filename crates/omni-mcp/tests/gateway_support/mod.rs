//! Gateway test fixtures: a scriptable upstream MCP server on loopback, a
//! recording webhook port, and a gateway wired like production.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use futures::future::BoxFuture;
use omni_core::clock::TestClock;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpClient, HttpConfig};
use omni_mcp::activity::ActivityRecorder;
use omni_mcp::events::service::McpEventService;
use omni_mcp::events::webhook::{WebhookDestination, WebhookError, WebhookEvent, WebhookPort};
use omni_mcp::gateway::access::DownstreamAccess;
use omni_mcp::gateway::bundled::BundledRegistry;
use omni_mcp::gateway::credentials::TokenRefresher;
use omni_mcp::gateway::secrets::GatewaySecrets;
use omni_mcp::gateway::{Gateway, GatewayParts};
use omni_mcp::rpc::McpProtocol;
use omni_runtime::ports::Ports;
use omni_store::Store;
use omni_testkit::TestStore;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt as _;

use crate::common;

pub const EVENTS_TOKEN: &str = "gateway-events-token-0123456789-ABCDEFGHIJ";

/// What the fake upstream does; every field may change mid-test.
pub struct FakeState {
    pub tools: Mutex<Vec<Value>>,
    pub page_size: Mutex<Option<usize>>,
    /// Pages returned since start, to check pagination happened.
    pub pages_served: AtomicUsize,
    pub resources: Mutex<Vec<Value>>,
    pub templates: Mutex<Vec<Value>>,
    pub resource_contents: Mutex<HashMap<String, Value>>,
    pub events: Mutex<Vec<Value>>,
    /// The upstream event log: `(seq, event)`.
    pub log: Mutex<Vec<(u64, Value)>>,
    pub truncate_next: AtomicBool,
    pub poll_failures: AtomicUsize,
    /// Bearer tokens accepted (none required when empty).
    pub tokens: Mutex<HashSet<String>>,
    pub sessions: Mutex<HashSet<String>>,
    pub expire_sessions: AtomicBool,
    /// Answer every request with this HTTP status.
    pub fail_status: Mutex<Option<u16>>,
    pub call_delay_ms: AtomicU64,
    pub results: Mutex<HashMap<String, Value>>,
    pub errors: Mutex<HashMap<String, Value>>,
    /// `(method, params, authorization)` of every request.
    pub requests: Mutex<Vec<(String, Value, Option<String>)>>,
    pub cancelled: Mutex<Vec<Value>>,
    pub active_calls: AtomicUsize,
    pub max_active_calls: AtomicUsize,
    /// Answer POSTs with an event stream (with a server `ping` first).
    pub stream_responses: AtomicBool,
    pub server_replies: Mutex<Vec<Value>>,
    pub offer_get_stream: AtomicBool,
    pub notify: broadcast::Sender<String>,
    pub capabilities: Mutex<Value>,
    pub initializes: AtomicUsize,
    /// Methods answered with a JSON-RPC error.
    pub fail_methods: Mutex<HashSet<String>>,
}

impl FakeState {
    fn new() -> Self {
        Self {
            tools: Mutex::new(vec![tool("echo", "Echo arguments", true)]),
            page_size: Mutex::new(None),
            pages_served: AtomicUsize::new(0),
            resources: Mutex::new(Vec::new()),
            templates: Mutex::new(Vec::new()),
            resource_contents: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            truncate_next: AtomicBool::new(false),
            poll_failures: AtomicUsize::new(0),
            tokens: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashSet::new()),
            expire_sessions: AtomicBool::new(false),
            fail_status: Mutex::new(None),
            call_delay_ms: AtomicU64::new(0),
            results: Mutex::new(HashMap::new()),
            errors: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            cancelled: Mutex::new(Vec::new()),
            active_calls: AtomicUsize::new(0),
            max_active_calls: AtomicUsize::new(0),
            stream_responses: AtomicBool::new(false),
            server_replies: Mutex::new(Vec::new()),
            offer_get_stream: AtomicBool::new(false),
            notify: broadcast::channel(16).0,
            capabilities: Mutex::new(json!({"tools": {"listChanged": true}})),
            initializes: AtomicUsize::new(0),
            fail_methods: Mutex::new(HashSet::new()),
        }
    }

    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| m == method)
            .map(|(_, p, _)| p.clone())
            .collect()
    }

    pub fn authorizations(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, a)| a.clone())
            .collect()
    }

    /// Appends an event to the upstream log.
    pub fn emit(&self, name: &str, data: Value) -> String {
        let mut log = self.log.lock().unwrap();
        let seq = log.len() as u64 + 1;
        let id = format!("up_{seq}");
        log.push((
            seq,
            json!({"eventId": id, "name": name, "timestamp": "2026-10-18T12:00:00.000Z", "data": data}),
        ));
        id
    }
}

pub fn tool(name: &str, description: &str, read_only: bool) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
        "annotations": {"readOnlyHint": read_only, "destructiveHint": !read_only},
    })
}

/// A running fake; dropped with the test.
pub struct FakeUpstream {
    pub url: String,
    pub state: Arc<FakeState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeUpstream {
    pub async fn start() -> Self {
        let state = Arc::new(FakeState::new());
        let router = Router::new()
            .route("/mcp", any(handle))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/mcp"),
            state,
            task,
        }
    }
}

fn rpc(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn json_reply(message: Value) -> Response {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        message.to_string(),
    )
        .into_response()
}

async fn handle(
    State(state): State<Arc<FakeState>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(status) = *state.fail_status.lock().unwrap() {
        return StatusCode::from_u16(status).unwrap().into_response();
    }
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    {
        let tokens = state.tokens.lock().unwrap();
        if !tokens.is_empty() {
            let ok = authorization
                .as_deref()
                .and_then(|a| a.strip_prefix("Bearer "))
                .is_some_and(|t| tokens.contains(t));
            if !ok {
                return (
                    StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer resource_metadata=\"https://upstream.example/.well-known/oauth-protected-resource\"")],
                )
                    .into_response();
            }
        }
    }
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if method == Method::GET {
        if !state.offer_get_stream.load(Ordering::Relaxed) {
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        let receiver = state.notify.subscribe();
        let stream = futures::stream::unfold(receiver, |mut receiver| async move {
            let method = receiver.recv().await.ok()?;
            let event = Event::default()
                .event("message")
                .data(json!({"jsonrpc": "2.0", "method": method}).to_string());
            Some((Ok::<_, std::convert::Infallible>(event), receiver))
        });
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }
    if method == Method::DELETE {
        if let Some(id) = session {
            state.sessions.lock().unwrap().remove(&id);
        }
        return StatusCode::OK.into_response();
    }
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let name = message["method"].as_str().unwrap_or_default().to_owned();
    let params = message.get("params").cloned().unwrap_or(json!({}));
    let id = message.get("id").cloned();
    if name.is_empty() {
        // A reply to a server request.
        state.server_replies.lock().unwrap().push(message.clone());
        return StatusCode::ACCEPTED.into_response();
    }
    if name == "initialize" {
        state
            .requests
            .lock()
            .unwrap()
            .push((name.clone(), params.clone(), authorization.clone()));
        state.initializes.fetch_add(1, Ordering::Relaxed);
        let session_id = omni_core::ids::uuid_v4();
        state.sessions.lock().unwrap().insert(session_id.clone());
        let result = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": *state.capabilities.lock().unwrap(),
            "serverInfo": {"name": "fake-upstream", "version": "1.2.3"},
        });
        let mut response = json_reply(rpc(id.as_ref().unwrap(), result));
        response
            .headers_mut()
            .insert("mcp-session-id", session_id.parse().unwrap());
        return response;
    }
    match &session {
        Some(session) if state.expire_sessions.load(Ordering::Relaxed) => {
            state.sessions.lock().unwrap().remove(session);
            state.expire_sessions.store(false, Ordering::Relaxed);
            return StatusCode::NOT_FOUND.into_response();
        }
        Some(session) if !state.sessions.lock().unwrap().contains(session) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Some(_) => {}
        None => return StatusCode::BAD_REQUEST.into_response(),
    }
    state
        .requests
        .lock()
        .unwrap()
        .push((name.clone(), params.clone(), authorization));
    if name == "notifications/cancelled" {
        state.cancelled.lock().unwrap().push(params);
        return StatusCode::ACCEPTED.into_response();
    }
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };
    let reply = answer(&state, &name, &params, &id).await;
    if state.stream_responses.load(Ordering::Relaxed) {
        let ping = json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"});
        let notice = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1, "progress": 1}});
        let body = format!(
            "event: message\ndata: {ping}\n\nevent: message\ndata: {notice}\n\nid: 7\nevent: message\ndata: {reply}\n\n"
        );
        return (
            StatusCode::OK,
            [("content-type", "text/event-stream")],
            body,
        )
            .into_response();
    }
    json_reply(reply)
}

async fn answer(state: &FakeState, method: &str, params: &Value, id: &Value) -> Value {
    if state.fail_methods.lock().unwrap().contains(method) {
        return rpc_error(id, -32603, "temporarily unavailable");
    }
    match method {
        "tools/list" => {
            let tools = state.tools.lock().unwrap().clone();
            let page = *state.page_size.lock().unwrap();
            state.pages_served.fetch_add(1, Ordering::Relaxed);
            match page {
                None => rpc(id, json!({"tools": tools})),
                Some(size) => {
                    let start: usize = params["cursor"]
                        .as_str()
                        .and_then(|c| c.parse().ok())
                        .unwrap_or(0);
                    let end = (start + size).min(tools.len());
                    let mut result = json!({"tools": tools[start..end]});
                    if end < tools.len() {
                        result["nextCursor"] = json!(end.to_string());
                    }
                    rpc(id, result)
                }
            }
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_owned();
            let active = state.active_calls.fetch_add(1, Ordering::SeqCst) + 1;
            state.max_active_calls.fetch_max(active, Ordering::SeqCst);
            let delay = state.call_delay_ms.load(Ordering::Relaxed);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            state.active_calls.fetch_sub(1, Ordering::SeqCst);
            if let Some(error) = state.errors.lock().unwrap().get(&name) {
                return json!({"jsonrpc": "2.0", "id": id, "error": error});
            }
            if !state
                .tools
                .lock()
                .unwrap()
                .iter()
                .any(|t| t["name"] == json!(name))
            {
                return rpc_error(id, -32602, &format!("Unknown tool {name}"));
            }
            let result = state
                .results
                .lock()
                .unwrap()
                .get(&name)
                .cloned()
                .unwrap_or_else(|| {
                    json!({"content": [{"type": "text", "text": format!("{name}: {}", params["arguments"])}]})
                });
            rpc(id, result)
        }
        "resources/list" => rpc(id, json!({"resources": *state.resources.lock().unwrap()})),
        "resources/templates/list" => rpc(
            id,
            json!({"resourceTemplates": *state.templates.lock().unwrap()}),
        ),
        "resources/read" => {
            let uri = params["uri"].as_str().unwrap_or_default();
            match state.resource_contents.lock().unwrap().get(uri) {
                Some(contents) => rpc(id, json!({"contents": [contents]})),
                None => rpc_error(id, -32002, "Resource not found"),
            }
        }
        "events/list" => rpc(id, json!({"events": *state.events.lock().unwrap()})),
        "events/poll" => {
            if state.poll_failures.load(Ordering::Relaxed) > 0 {
                state.poll_failures.fetch_sub(1, Ordering::Relaxed);
                return rpc_error(id, -32603, "temporarily unavailable");
            }
            let log = state.log.lock().unwrap();
            let latest = log.len() as u64;
            let cursor: Option<u64> = params["cursor"].as_str().and_then(|c| c.parse().ok());
            let Some(from) = cursor else {
                return rpc(
                    id,
                    json!({"events": [], "cursor": latest.to_string(), "hasMore": false, "truncated": false, "nextPollMs": 1000}),
                );
            };
            let max = params["maxEvents"].as_u64().unwrap_or(50) as usize;
            let after: Vec<&(u64, Value)> = log.iter().filter(|(seq, _)| *seq > from).collect();
            let page: Vec<Value> = after.iter().take(max).map(|(_, e)| e.clone()).collect();
            let last = after
                .iter()
                .take(max)
                .next_back()
                .map_or(from, |(seq, _)| *seq);
            let truncated = state.truncate_next.swap(false, Ordering::Relaxed);
            rpc(
                id,
                json!({"events": page, "cursor": last.to_string(), "hasMore": after.len() > max, "truncated": truncated, "nextPollMs": 1000}),
            )
        }
        "ping" => rpc(id, json!({})),
        _ => rpc_error(id, -32601, "Method not found"),
    }
}

/// Records every delivery; verification always succeeds.
#[derive(Default)]
pub struct RecordingWebhook {
    pub delivered: Mutex<Vec<WebhookEvent>>,
    pub fail: AtomicBool,
}

impl WebhookPort for RecordingWebhook {
    fn verify<'a>(&'a self, _: &'a WebhookDestination) -> BoxFuture<'a, Result<(), WebhookError>> {
        Box::pin(async { Ok(()) })
    }

    fn deliver<'a>(
        &'a self,
        _: &'a WebhookDestination,
        event: &'a WebhookEvent,
    ) -> BoxFuture<'a, Result<u16, WebhookError>> {
        Box::pin(async move {
            if self.fail.load(Ordering::Relaxed) {
                return Ok(503);
            }
            self.delivered.lock().unwrap().push(event.clone());
            Ok(204)
        })
    }
}

/// A gateway over a store, like production wiring (loopback allowed).
pub struct Fixture {
    pub store: Store,
    pub clock: Arc<TestClock>,
    pub gateway: Gateway,
    pub access: DownstreamAccess,
    pub events: McpEventService,
    pub webhook: Arc<RecordingWebhook>,
    pub router: Router,
    /// The `/api/gateway` management routes and the OAuth callback.
    pub admin: Router,
    pub shutdown: CancellationToken,
    pub tracker: TaskTracker,
    _store: Option<TestStore>,
}

#[derive(Default)]
pub struct Options {
    pub bundled: BundledRegistry,
    pub refresher: Option<Arc<dyn TokenRefresher>>,
    /// `SideEffectMode::Record`.
    pub record: bool,
}

pub fn secrets() -> GatewaySecrets {
    GatewaySecrets::from_key([42; 32])
}

impl Fixture {
    pub async fn new() -> Self {
        let clock = common::clock();
        let store = common::test_store(&clock).await;
        let mut fixture = Self::over(store.store.clone(), clock, Options::default()).await;
        fixture._store = Some(store);
        fixture
    }

    pub async fn with(options: Options) -> Self {
        let clock = common::clock();
        let store = common::test_store(&clock).await;
        let mut fixture = Self::over(store.store.clone(), clock, options).await;
        fixture._store = Some(store);
        fixture
    }

    /// A fresh process over an existing store (a restart).
    pub async fn over(store: Store, clock: Arc<TestClock>, options: Options) -> Self {
        let http = HttpClient::new(HttpConfig::default()).unwrap();
        let public = PublicHttpClient::new(&http).allow_loopback_for_tests();
        let oauth = omni_mcp::gateway::oauth::client::OAuthClient::new(public.clone());
        let tracker = TaskTracker::new();
        let shutdown = CancellationToken::new();
        let recorder = ActivityRecorder::new(store.clone(), clock.clone(), tracker.clone());
        let gateway = Gateway::new(GatewayParts {
            store: store.clone(),
            clock: clock.clone(),
            http: public,
            secrets: secrets(),
            refresher: options.refresher,
            bundled: options.bundled,
            tracker: tracker.clone(),
            shutdown: shutdown.clone(),
            recorder: recorder.clone(),
            allow_loopback_http: true,
            side_effects: if options.record {
                omni_http::SideEffectMode::Record
            } else {
                omni_http::SideEffectMode::Live
            },
        });
        let access =
            DownstreamAccess::new(store.clone(), clock.clone(), secrets(), tracker.clone());
        let webhook = Arc::new(RecordingWebhook::default());
        let events = McpEventService::new(
            EVENTS_TOKEN,
            store.clone(),
            clock.clone(),
            webhook.clone(),
            Ports::default(),
        );
        omni_mcp::gateway::events::extend_events(&events, &gateway, &access);
        let protocol = McpProtocol::gateway(
            common::other_tools(&[]),
            recorder,
            Some(events.clone()),
            gateway.clone(),
        )
        .unwrap();
        let router = omni_mcp::gateway::endpoint::router(
            protocol,
            access.clone(),
            gateway.clone(),
            Some("https://gateway.example/.well-known/oauth-protected-resource/gateway/mcp".into()),
            shutdown.clone(),
        );
        let admin = omni_mcp::gateway::routes::router(omni_mcp::gateway::routes::AdminState {
            gateway: gateway.clone(),
            access: access.clone(),
            events: Some(events.clone()),
            oauth,
            authorization: None,
            public_url: None,
        });
        gateway.load().await.unwrap();
        Self {
            store,
            clock,
            gateway,
            access,
            events,
            webhook,
            router,
            admin,
            shutdown,
            tracker,
            _store: None,
        }
    }

    pub async fn client_token(&self) -> String {
        self.access.create_client("test").await.unwrap().1
    }

    /// Adds an HTTP integration and refreshes it.
    pub async fn add(&self, id: &str, url: &str, auth: omni_api::gateway::AuthKind) {
        self.gateway
            .create(omni_api::gateway::IntegrationCreate {
                id: id.into(),
                name: format!("{id} name"),
                transport: omni_api::gateway::TransportInput::Http { url: url.into() },
                auth,
                timeout_seconds: Some(5),
                enabled: Some(true),
            })
            .await
            .unwrap();
    }

    pub async fn refresh(&self, id: &str) -> omni_mcp::gateway::model::RefreshRow {
        self.gateway.refresh_now(id).await.unwrap()
    }

    pub async fn post(&self, token: &str, body: &Value) -> common::Exchange {
        let request = Request::builder()
            .method("POST")
            .uri("/gateway/mcp")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .body(Body::from(body.to_string()))
            .unwrap();
        common::send(&self.router, request).await
    }

    pub async fn call(&self, token: &str, name: &str, arguments: Value) -> Value {
        self.post(
            token,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}),
        )
        .await
        .message()
    }

    pub async fn tool_names(&self, token: &str) -> Vec<String> {
        let listed = self
            .post(
                token,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            )
            .await
            .message();
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        self.gateway.close().await;
        self.tracker.close();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.tracker.wait()).await;
    }
}

/// The router's raw response for a request (status and headers).
pub async fn raw(router: &Router, request: Request<Body>) -> Response {
    router.clone().oneshot(request).await.unwrap()
}

pub fn subscribe_params(name: &str, arguments: Value) -> Value {
    json!({
        "name": name,
        "arguments": arguments,
        "delivery": {"mode": "webhook", "url": "https://hooks.example/omni", "secret": format!("whsec_{}", base64_secret())},
    })
}

fn base64_secret() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode([7u8; 32])
}

pub fn object(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}
