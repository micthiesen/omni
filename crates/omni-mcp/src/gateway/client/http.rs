//! Streamable HTTP sessions (MCP 2025-03-26 and later).
//!
//! Every request is a POST answered by JSON or by a per-request event stream;
//! an optional standalone GET stream carries list-changed notifications.
//! Requests go through the SSRF-guarded public client without redirects:
//! every URL and DNS answer must be public. A 404 on a request that carried
//! a session id means the session expired and the request was not processed.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_http::public::PublicHttpClient;
use omni_http::{HeaderName, HeaderValue, HttpError, Method, RedirectRule, Response, Url};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::sse::SseDecoder;
use super::{
    CONNECT_TIMEOUT, CancelOnDrop, ClientError, Incoming, MAX_MESSAGE_BYTES, NoticeSender,
    RequestOptions, RpcResult, Session, UpstreamNotice, answer_server_request, cancel_reason,
    cancelled_params, classify, initialize_params, negotiated_version, notice_for,
};

const LOG: &str = "MCP:Gateway";
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const ACCEPT_BOTH: &str = "application/json, text/event-stream";
const MAX_ERROR_BODY: usize = 4_096;
const STREAM_BACKOFF: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// Request headers that carry credentials; replaced when OAuth refreshes.
pub type AuthHeaders = Vec<(HeaderName, HeaderValue)>;

struct Inner {
    http: PublicHttpClient,
    url: Url,
    headers: RwLock<AuthHeaders>,
    session_id: Mutex<Option<String>>,
    version: String,
    initialized: Value,
    next_id: AtomicI64,
    notices: NoticeSender,
    alive: AtomicBool,
    stop: CancellationToken,
    tracker: TaskTracker,
}

/// One Streamable HTTP session; cheap to clone.
#[derive(Clone)]
pub struct HttpSession {
    inner: Arc<Inner>,
}

impl HttpSession {
    /// Runs `initialize` and `notifications/initialized`, then opens the
    /// optional GET stream.
    pub async fn connect(
        http: PublicHttpClient,
        url: Url,
        headers: AuthHeaders,
        notices: NoticeSender,
        tracker: TaskTracker,
        stop: CancellationToken,
    ) -> Result<Self, ClientError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": initialize_params(),
        });
        let mut pending = Inner {
            http,
            url,
            headers: RwLock::new(headers),
            session_id: Mutex::new(None),
            version: String::new(),
            initialized: Value::Null,
            next_id: AtomicI64::new(1),
            notices,
            alive: AtomicBool::new(true),
            stop,
            tracker,
        };
        let options = RequestOptions::new(CONNECT_TIMEOUT);
        let initialize = tokio::time::timeout(
            CONNECT_TIMEOUT,
            pending.post(&body, Some(0), &options, true),
        );
        let (response, session) = match initialize.await.unwrap_or(Err(ClientError::Timeout)) {
            Ok(done) => done,
            Err(ClientError::Http { status, .. }) if matches!(status, 404 | 405) => {
                return Err(ClientError::Unsupported(format!(
                    "the integration answered HTTP {status} to a Streamable HTTP POST; it may \
                     only offer the deprecated HTTP+SSE transport, which Omni does not support"
                )));
            }
            Err(error) => return Err(error),
        };
        let result = match response {
            Ok(result) => result,
            Err(error) => {
                return Err(ClientError::Protocol(format!(
                    "initialize failed: {} ({})",
                    error.message, error.code
                )));
            }
        };
        pending.version = negotiated_version(&result)?;
        pending.initialized = result;
        *pending
            .session_id
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = session;
        let session = Self {
            inner: Arc::new(pending),
        };
        let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        session
            .inner
            .post(
                &initialized,
                None,
                &RequestOptions::new(CONNECT_TIMEOUT),
                false,
            )
            .await
            .map(|_accepted| ())?;
        let stream = session.clone();
        omni_core::spawn::spawn_tracked(
            &session.inner.tracker,
            "mcp-gateway-get-stream",
            async move { stream.inner.listen().await },
        );
        Ok(session)
    }

    /// Replaces the credential headers (after an OAuth refresh).
    pub fn set_headers(&self, headers: AuthHeaders) {
        *self
            .inner
            .headers
            .write()
            .unwrap_or_else(PoisonError::into_inner) = headers;
    }
}

fn header_value(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn http_error(error: HttpError) -> ClientError {
    match error {
        HttpError::Timeout => ClientError::Timeout,
        HttpError::Network(message) => ClientError::Network(message),
        HttpError::Blocked(message) | HttpError::InvalidUrl(message) => {
            ClientError::Blocked(message)
        }
        HttpError::Status { status, body } => ClientError::Http { status, body },
        HttpError::TooLarge { limit } => ClientError::TooLarge(limit),
        HttpError::Decode(message) => ClientError::Protocol(message),
    }
}

/// A body read failure, described without the URL.
fn network(error: impl Into<Box<dyn std::error::Error>>) -> ClientError {
    ClientError::Network(omni_http::error_chain(error.into().as_ref()))
}

async fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, ClientError> {
    if response
        .content_length()
        .is_some_and(|declared| declared > limit as u64)
    {
        return Err(ClientError::TooLarge(limit));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| network(e.without_url()))?
    {
        if body.len() + chunk.len() > limit {
            return Err(ClientError::TooLarge(limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn is_event_stream(response: &Response) -> bool {
    header_value(response, "content-type").is_some_and(|value| {
        value
            .split(';')
            .next()
            .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("text/event-stream"))
    })
}

impl Inner {
    fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn sends_version_header(&self) -> bool {
        !self.version.is_empty() && !matches!(self.version.as_str(), "2025-03-26" | "2024-11-05")
    }

    fn request(&self, method: Method, accept: &str) -> omni_http::RequestBuilder {
        let mut builder = self
            .http
            .request(method, self.url.clone())
            .redirect(RedirectRule::Error)
            .header("accept", accept);
        if let Some(id) = self.session_id() {
            builder = builder.header(SESSION_HEADER, id);
        }
        if self.sends_version_header() {
            builder = builder.header(VERSION_HEADER, self.version.clone());
        }
        let headers = self
            .headers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
    }

    /// POSTs one message. With `expect` set, waits for that response id
    /// (from JSON or the event stream); otherwise expects 202. Returns the
    /// outcome and, during `initialize`, the session id the server assigned.
    async fn post(
        &self,
        body: &Value,
        expect: Option<i64>,
        options: &RequestOptions,
        initializing: bool,
    ) -> Result<(Result<Value, super::RpcError>, Option<String>), ClientError> {
        let had_session = self.session_id().is_some();
        let request = self
            .request(Method::POST, ACCEPT_BOTH)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(body).unwrap_or_default())
            .timeout(options.timeout);
        let response = request.send_streaming().await.map_err(http_error)?;
        let status = response.status().as_u16();
        let www_authenticate = header_value(&response, "www-authenticate");
        match status {
            401 => return Err(ClientError::Unauthorized { www_authenticate }),
            403 => return Err(ClientError::Forbidden { www_authenticate }),
            404 if had_session => {
                self.alive.store(false, Ordering::Relaxed);
                return Err(ClientError::SessionExpired);
            }
            200..=299 => {}
            _ => {
                let body = read_bounded(response, MAX_ERROR_BODY)
                    .await
                    .unwrap_or_default();
                return Err(ClientError::Http {
                    status,
                    body: String::from_utf8_lossy(&body).chars().take(500).collect(),
                });
            }
        }
        let session = if initializing {
            header_value(&response, SESSION_HEADER).filter(|id| {
                !id.is_empty() && id.len() <= 512 && id.chars().all(|c| c.is_ascii_graphic())
            })
        } else {
            None
        };
        let Some(expect) = expect else {
            return Ok((Ok(Value::Null), session));
        };
        let outcome = if is_event_stream(&response) {
            self.await_in_stream(response, expect).await?
        } else {
            let body = read_bounded(response, MAX_MESSAGE_BYTES).await?;
            let message: Value = serde_json::from_slice(&body)
                .map_err(|e| ClientError::Protocol(format!("invalid JSON response: {e}")))?;
            let messages = match message {
                Value::Array(items) => items,
                single => vec![single],
            };
            let mut found = None;
            for message in messages {
                if let Incoming::Response { id, outcome } = classify(&message)
                    && id.as_i64() == Some(expect)
                {
                    found = Some(outcome);
                }
            }
            found.ok_or_else(|| {
                ClientError::Protocol("the response did not answer the request".into())
            })?
        };
        Ok((outcome, session))
    }

    /// Reads a per-request event stream until the response with `expect`.
    async fn await_in_stream(
        &self,
        mut response: Response,
        expect: i64,
    ) -> Result<Result<Value, super::RpcError>, ClientError> {
        let mut decoder = SseDecoder::new(MAX_MESSAGE_BYTES);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| network(e.without_url()))?
        {
            let events = decoder
                .push(&chunk)
                .map_err(|_| ClientError::TooLarge(MAX_MESSAGE_BYTES))?;
            for event in events.into_iter().filter(|e| e.is_message()) {
                let Ok(message) = serde_json::from_str::<Value>(&event.data) else {
                    continue;
                };
                match classify(&message) {
                    Incoming::Response { id, outcome } if id.as_i64() == Some(expect) => {
                        return Ok(outcome);
                    }
                    other => self.handle_incoming(other),
                }
            }
        }
        Err(ClientError::Protocol(
            "the event stream ended before the response".into(),
        ))
    }

    /// Forwards notices and answers server requests (in the background).
    fn handle_incoming(&self, incoming: Incoming) {
        match incoming {
            Incoming::Notification { method } => {
                if let Some(notice) = notice_for(&method) {
                    let _ = self.notices.send(notice);
                }
            }
            Incoming::Request { id, method } => {
                let reply = answer_server_request(id, &method);
                let post = self
                    .request(Method::POST, ACCEPT_BOTH)
                    .header("content-type", "application/json")
                    .body(serde_json::to_vec(&reply).unwrap_or_default())
                    .timeout(Duration::from_secs(10));
                omni_core::spawn::spawn_tracked(
                    &self.tracker,
                    "mcp-gateway-server-reply",
                    async move {
                        if let Err(error) = post.send_streaming().await {
                            tracing::debug!(target: LOG, %error, "Server request reply failed");
                        }
                    },
                );
            }
            Incoming::Response { .. } | Incoming::Invalid => {}
        }
    }

    /// The standalone GET stream, reconnected with backoff until the
    /// session closes. Servers that do not offer one (405) end it at once.
    async fn listen(&self) {
        let mut failures = 0usize;
        let mut last_event: Option<String> = None;
        loop {
            if self.stop.is_cancelled() || !self.alive.load(Ordering::Relaxed) {
                return;
            }
            let mut request = self.request(Method::GET, "text/event-stream");
            if let Some(id) = &last_event {
                request = request.header("last-event-id", id.clone());
            }
            let response = tokio::select! {
                () = self.stop.cancelled() => return,
                sent = request.timeout(CONNECT_TIMEOUT).send_streaming() => sent,
            };
            match response {
                Ok(response) if response.status().is_success() && is_event_stream(&response) => {
                    let received = tokio::select! {
                        () = self.stop.cancelled() => return,
                        received = self.read_stream(response, &mut last_event) => received,
                    };
                    if received {
                        failures = 0;
                    }
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    if matches!(status, 400 | 401 | 403 | 404 | 405 | 406) || status < 300 {
                        return;
                    }
                }
                Err(_) => {}
            }
            let delay = STREAM_BACKOFF[failures.min(STREAM_BACKOFF.len() - 1)];
            failures += 1;
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Reads one GET stream; whether it delivered anything.
    async fn read_stream(&self, mut response: Response, last_event: &mut Option<String>) -> bool {
        let mut decoder = SseDecoder::new(MAX_MESSAGE_BYTES);
        let mut received = false;
        while let Ok(Some(chunk)) = response.chunk().await {
            let Ok(events) = decoder.push(&chunk) else {
                return received;
            };
            for event in events {
                received = true;
                if event.id.is_some() {
                    last_event.clone_from(&event.id);
                }
                if !event.is_message() {
                    continue;
                }
                if let Ok(message) = serde_json::from_str::<Value>(&event.data) {
                    self.handle_incoming(classify(&message));
                }
            }
        }
        received
    }
}

impl Session for HttpSession {
    fn initialized(&self) -> &Value {
        &self.inner.initialized
    }

    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Map<String, Value>,
        options: RequestOptions,
    ) -> BoxFuture<'a, RpcResult> {
        Box::pin(async move {
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let session = self.clone();
            let mut guard = CancelOnDrop::new(move |reason: &'static str| {
                let tracker = session.inner.tracker.clone();
                omni_core::spawn::spawn_tracked(&tracker, "mcp-gateway-cancel", async move {
                    session
                        .notify("notifications/cancelled", cancelled_params(id, reason))
                        .await;
                });
            });
            let sent = self.inner.post(&body, Some(id), &options, false);
            let outcome = tokio::select! {
                () = options.cancel.cancelled() => Err(ClientError::Cancelled),
                outcome = tokio::time::timeout(options.timeout, sent) => match outcome {
                    Ok(outcome) => outcome.map(|(outcome, _)| outcome),
                    Err(_) => Err(ClientError::Timeout),
                },
            };
            if matches!(outcome, Err(ClientError::Cancelled | ClientError::Timeout)) {
                guard.fire(cancel_reason(&options));
            } else {
                guard.disarm();
            }
            outcome
        })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Map<String, Value>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
            let options = RequestOptions::new(Duration::from_secs(10));
            let sent = tokio::time::timeout(
                options.timeout,
                self.inner.post(&body, None, &options, false),
            );
            if let Err(error) = sent.await.unwrap_or(Err(ClientError::Timeout)) {
                tracing::debug!(target: LOG, %error, method, "Upstream notification failed");
            }
        })
    }

    fn alive(&self) -> bool {
        self.inner.alive.load(Ordering::Relaxed)
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.inner.alive.store(false, Ordering::Relaxed);
            self.inner.stop.cancel();
            let _ = self.inner.notices.send(UpstreamNotice::Closed);
            if self.inner.session_id().is_some() {
                let request = self
                    .inner
                    .request(Method::DELETE, ACCEPT_BOTH)
                    .timeout(Duration::from_secs(5));
                if let Err(error) = request.send_streaming().await {
                    tracing::debug!(target: LOG, %error, "Session DELETE failed");
                }
            }
        })
    }
}
