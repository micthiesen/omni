//! Upstream MCP client sessions.
//!
//! The gateway speaks the stateful 2025 protocol revisions to every
//! integration (`initialize`, `notifications/initialized`, then requests),
//! over Streamable HTTP ([`http`]) or a bundled stdio server ([`stdio`]).
//! Results are kept as raw JSON, so content blocks, structured content,
//! `_meta` and `isError` reach the downstream client unchanged.
//!
//! Server-to-client requests are answered conservatively: `ping` with `{}`,
//! `roots/list` with no roots, `elicitation/create` with a decline, and
//! everything else (sampling) with method-not-found. Notifications that
//! change a catalog are forwarded as [`UpstreamNotice`].

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub mod http;
pub mod sse;
pub mod stdio;

/// The revision Omni asks for.
pub const CLIENT_PROTOCOL_VERSION: &str = "2025-11-25";
/// Revisions Omni accepts from an integration's `initialize`.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// One upstream JSON-RPC message (and one response body) is at most this.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Connecting and `initialize` together.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A JSON-RPC error returned by the integration.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn from_value(value: &Value) -> Self {
        Self {
            code: value.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Upstream error")
                .chars()
                .take(2_000)
                .collect(),
            data: value
                .get("data")
                .filter(|data| data.to_string().len() <= 8_192)
                .cloned(),
        }
    }
}

/// A transport-level failure.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("the integration requires authentication")]
    Unauthorized { www_authenticate: Option<String> },
    #[error("the integration refused access (HTTP 403)")]
    Forbidden { www_authenticate: Option<String> },
    #[error("the integration's session expired")]
    SessionExpired,
    #[error("the integration did not answer in time")]
    Timeout,
    #[error("the request was cancelled")]
    Cancelled,
    #[error("network error: {0}")]
    Network(String),
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("the integration's response exceeds {0} bytes")]
    TooLarge(usize),
    #[error("{0}")]
    Process(String),
    #[error("blocked: {0}")]
    Blocked(String),
    #[error("{0}")]
    Unsupported(String),
}

impl ClientError {
    /// The integration certainly did not act on the request, so it may be
    /// sent again (after re-authenticating or reconnecting).
    pub fn not_processed(&self) -> bool {
        matches!(
            self,
            ClientError::Unauthorized { .. }
                | ClientError::Forbidden { .. }
                | ClientError::SessionExpired
                | ClientError::Blocked(_)
                | ClientError::Unsupported(_)
        ) || matches!(self, ClientError::Http { status, .. } if *status < 500)
    }
}

/// Every request's outcome: the result, or the integration's JSON-RPC error.
pub type RpcResult = Result<Result<Value, RpcError>, ClientError>;

/// Per-request bounds.
#[derive(Clone, Debug)]
pub struct RequestOptions {
    pub timeout: Duration,
    pub cancel: CancellationToken,
}

impl RequestOptions {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            cancel: CancellationToken::new(),
        }
    }
}

/// A catalog-relevant notification from an integration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamNotice {
    ToolsChanged,
    ResourcesChanged,
    /// The session ended (a stdio server exited, a stream failed for good).
    Closed,
}

/// An initialized session.
pub trait Session: Send + Sync {
    /// The integration's `initialize` result.
    fn initialized(&self) -> &Value;
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Map<String, Value>,
        options: RequestOptions,
    ) -> BoxFuture<'a, RpcResult>;
    /// Best-effort `notifications/*` (cancellation).
    fn notify<'a>(&'a self, method: &'a str, params: Map<String, Value>) -> BoxFuture<'a, ()>;
    /// Whether the session can still serve requests.
    fn alive(&self) -> bool;
    fn close(&self) -> BoxFuture<'_, ()>;
}

pub type NoticeSender = mpsc::UnboundedSender<UpstreamNotice>;

/// The `initialize` request body.
pub fn initialize_params() -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("protocolVersion".into(), json!(CLIENT_PROTOCOL_VERSION));
    params.insert("capabilities".into(), json!({}));
    params.insert(
        "clientInfo".into(),
        json!({"name": "omni-gateway", "title": "Omni MCP gateway", "version": env!("CARGO_PKG_VERSION")}),
    );
    params
}

/// Checks an `initialize` result and returns the negotiated revision.
pub fn negotiated_version(result: &Value) -> Result<String, ClientError> {
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| ClientError::Protocol("initialize result has no protocolVersion".into()))?;
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
        Ok(version.to_owned())
    } else {
        Err(ClientError::Unsupported(format!(
            "the integration speaks MCP {version}; Omni supports {}",
            SUPPORTED_PROTOCOL_VERSIONS.join(", ")
        )))
    }
}

/// What a message from the integration is.
pub enum Incoming {
    Response {
        id: Value,
        outcome: Result<Value, RpcError>,
    },
    Request {
        id: Value,
        method: String,
    },
    Notification {
        method: String,
    },
    Invalid,
}

pub fn classify(message: &Value) -> Incoming {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").filter(|id| !id.is_null()).cloned();
    match (method, id) {
        (Some(method), Some(id)) => Incoming::Request {
            id,
            method: method.to_owned(),
        },
        (Some(method), None) => Incoming::Notification {
            method: method.to_owned(),
        },
        (None, Some(id)) => match (message.get("result"), message.get("error")) {
            (Some(result), _) => Incoming::Response {
                id,
                outcome: Ok(result.clone()),
            },
            (None, Some(error)) => Incoming::Response {
                id,
                outcome: Err(RpcError::from_value(error)),
            },
            _ => Incoming::Invalid,
        },
        (None, None) => Incoming::Invalid,
    }
}

/// The reply to a server-to-client request.
pub fn answer_server_request(id: Value, method: &str) -> Value {
    match method {
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": []}}),
        "elicitation/create" => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"action": "decline"}})
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "Method not supported by the Omni gateway"}
        }),
    }
}

/// The notice a server notification maps to.
pub fn notice_for(method: &str) -> Option<UpstreamNotice> {
    match method {
        "notifications/tools/list_changed" => Some(UpstreamNotice::ToolsChanged),
        "notifications/resources/list_changed" => Some(UpstreamNotice::ResourcesChanged),
        _ => None,
    }
}

/// `notifications/cancelled` params for request `id`.
pub fn cancelled_params(id: i64, reason: &str) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("requestId".into(), json!(id));
    params.insert("reason".into(), json!(reason));
    params
}

/// Sends `notifications/cancelled` for a request that did not finish: it
/// was cancelled, timed out, or its future was dropped because the
/// downstream client went away.
pub struct CancelOnDrop<F: FnOnce(&'static str)> {
    send: Option<F>,
}

impl<F: FnOnce(&'static str)> CancelOnDrop<F> {
    pub fn new(send: F) -> Self {
        Self { send: Some(send) }
    }

    /// The request finished; nothing to cancel.
    pub fn disarm(&mut self) {
        self.send = None;
    }

    pub fn fire(&mut self, reason: &'static str) {
        if let Some(send) = self.send.take() {
            send(reason);
        }
    }
}

impl<F: FnOnce(&'static str)> Drop for CancelOnDrop<F> {
    fn drop(&mut self) {
        self.fire("The client went away");
    }
}

/// Why an unfinished request is cancelled.
pub fn cancel_reason(options: &RequestOptions) -> &'static str {
    if options.cancel.is_cancelled() {
        "The client cancelled the request"
    } else {
        "The Omni gateway timed out"
    }
}
