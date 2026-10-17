//! Dashboard snapshot and the `/api/events` SSE hub.
//!
//! - A new client gets a freshly built snapshot (never a replay of the last
//!   broadcast) enqueued under the broadcast lock, so a newer broadcast can
//!   never be overtaken by an older initial frame.
//! - Bus changes (task runs, streamer updates, data deletions) are debounced
//!   by 150 ms, then one snapshot is built for every client; a broadcast within
//!   150 ms of the previous one is skipped, and an unchanged payload is not
//!   sent again.
//! - Every client gets a `ping` frame (data: epoch ms) on connect, before
//!   its initial snapshot, and every 25 s after.
//! - Snapshot frame ids increase monotonically across all clients.
//! - Every snapshot carries the process's build identity, computed once, so a
//!   page reconnecting after a deploy sees the new build.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt as _};
use omni_api::build::BuildIdentity;
use omni_api::snapshot::SNAPSHOT_RUN_LIMIT;
use omni_runtime::AppContext;
use omni_runtime::ports::PortError;
use omni_store::StoreError;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::OpsState;
use crate::json::js_json;

const LOG: &str = "Main:Server";
/// `SSE_DEBOUNCE_MS`.
pub const SSE_DEBOUNCE: Duration = Duration::from_millis(150);
/// `SSE_HEARTBEAT_MS`.
pub const SSE_HEARTBEAT: Duration = Duration::from_secs(25);

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Default)]
struct HubState {
    last_broadcast: Option<String>,
    last_broadcast_at: Option<Instant>,
    clients: Vec<mpsc::UnboundedSender<Event>>,
}

struct Inner {
    ctx: AppContext,
    build: BuildIdentity,
    /// Serializes broadcasts and initial frames.
    lock: tokio::sync::Mutex<()>,
    state: Mutex<HubState>,
    next_id: AtomicU64,
}

/// The dashboard hub; cheap to clone.
#[derive(Clone)]
pub struct Dashboard {
    inner: Arc<Inner>,
}

impl Dashboard {
    pub fn new(ctx: AppContext) -> Self {
        Self {
            inner: Arc::new(Inner {
                build: crate::build_identity::current(&ctx.paths.web_dist),
                ctx,
                lock: tokio::sync::Mutex::new(()),
                state: Mutex::new(HubState::default()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Connected SSE clients.
    pub fn client_count(&self) -> usize {
        let mut state = self.state();
        state.clients.retain(|c| !c.is_closed());
        state.clients.len()
    }

    /// This process's build identity.
    pub fn build(&self) -> &BuildIdentity {
        &self.inner.build
    }

    /// `{tasks, streamers, runs, onDeck, build}`.
    pub async fn snapshot(&self) -> Result<Value, SnapshotError> {
        let ctx = &self.inner.ctx;
        let tasks = serde_json::to_value(ctx.tasks.list().await?)?;
        let streamers = match ctx.ports.live_directory() {
            Some(directory) => directory.display().await?,
            None => Vec::new(),
        };
        let runs: Vec<omni_api::runs::Run> =
            omni_tasks::persistence::get_runs(&ctx.store, None, SNAPSHOT_RUN_LIMIT)
                .await?
                .iter()
                .map(Into::into)
                .collect();
        let on_deck = match ctx.ports.on_deck_source() {
            Some(source) => source.on_deck().await?,
            None => Vec::new(),
        };
        let mut snapshot = Map::new();
        snapshot.insert("tasks".to_owned(), tasks);
        snapshot.insert("streamers".to_owned(), Value::Array(streamers));
        snapshot.insert("runs".to_owned(), serde_json::to_value(runs)?);
        snapshot.insert("onDeck".to_owned(), Value::Array(on_deck));
        snapshot.insert("build".to_owned(), serde_json::to_value(self.build())?);
        Ok(omni_core::js::normalize_numbers(Value::Object(snapshot)))
    }

    async fn snapshot_payload(&self) -> Result<String, SnapshotError> {
        Ok(omni_core::js::json_stringify(&self.snapshot().await?))
    }

    fn next_id(&self) -> u64 {
        self.inner.next_id.fetch_add(1, Ordering::SeqCst)
    }

    pub async fn broadcast(&self) {
        let _held = self.inner.lock.lock().await;
        let now = Instant::now();
        {
            let mut state = self.state();
            if state
                .last_broadcast_at
                .is_some_and(|at| now.duration_since(at) < SSE_DEBOUNCE)
            {
                return;
            }
            state.last_broadcast_at = Some(now);
        }
        let payload = match self.snapshot_payload().await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(target: LOG, error = %error, "Dashboard snapshot failed");
                return;
            }
        };
        let mut state = self.state();
        if state.last_broadcast.as_deref() == Some(payload.as_str()) {
            return;
        }
        let id = self.next_id();
        let frame = omni_server_kit::sse::event("snapshot", id, &payload);
        state.last_broadcast = Some(payload);
        state
            .clients
            .retain(|client| client.send(frame.clone()).is_ok());
    }

    /// Registers a client: its first frame is a freshly built snapshot.
    pub async fn connect(&self) -> Result<mpsc::UnboundedReceiver<Event>, SnapshotError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _held = self.inner.lock.lock().await;
        let payload = self.snapshot_payload().await?;
        let id = self.next_id();
        let mut state = self.state();
        // The receiver is alive: this send cannot fail.
        let _ = tx.send(omni_server_kit::sse::event("snapshot", id, &payload));
        state.clients.push(tx);
        Ok(rx)
    }

    /// Debounces bus changes into broadcasts until `shutdown`.
    pub async fn listen(self) {
        let ctx = self.inner.ctx.clone();
        let mut runs = ctx.bus.task_runs();
        let mut app = ctx.bus.app();
        loop {
            tokio::select! {
                () = ctx.shutdown.cancelled() => return,
                changed = next_change(&mut runs, &mut app) => if !changed { return },
            }
            // Trailing debounce: wait for 150 ms without another change.
            loop {
                tokio::select! {
                    () = ctx.shutdown.cancelled() => return,
                    () = tokio::time::sleep(SSE_DEBOUNCE) => break,
                    changed = next_change(&mut runs, &mut app) => if !changed { return },
                }
            }
            if self.client_count() > 0 {
                self.broadcast().await;
            }
        }
    }
}

/// Waits for the next task-run or app event; `false` once both buses close.
async fn next_change(
    runs: &mut broadcast::Receiver<omni_tasks::TaskRunEvent>,
    app: &mut broadcast::Receiver<omni_tasks::AppEvent>,
) -> bool {
    use broadcast::error::RecvError;
    loop {
        tokio::select! {
            received = runs.recv() => match received {
                Ok(_) | Err(RecvError::Lagged(_)) => return true,
                Err(RecvError::Closed) => {}
            },
            received = app.recv() => match received {
                Ok(_) | Err(RecvError::Lagged(_)) => return true,
                Err(RecvError::Closed) => {}
            },
        }
        if runs.is_closed() && app.is_closed() {
            return false;
        }
    }
}

/// The `X-Accel-Buffering: no` header every SSE response carries.
pub fn with_sse_headers(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    for (name, value) in omni_server_kit::sse::HEADERS {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

/// Ends `stream` at shutdown so open SSE connections never hold the process.
pub fn until_shutdown<S>(
    stream: S,
    shutdown: tokio_util::sync::CancellationToken,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    stream.take_until(async move { shutdown.cancelled().await })
}

pub async fn snapshot_route(State(state): State<OpsState>) -> Response {
    match state.dashboard.snapshot().await {
        Ok(snapshot) => js_json(StatusCode::OK, &snapshot),
        Err(error) => omni_server_kit::ApiError::internal(error).into_response(),
    }
}

/// The response starts at once: a `ping` goes out
/// immediately, then the client's own initial snapshot, then broadcasts. A
/// failed initial snapshot is logged and ends the stream (the frontend keeps
/// polling and reconnects).
pub async fn events_route(State(state): State<OpsState>) -> Response {
    let dashboard = state.dashboard.clone();
    let snapshots =
        futures::stream::once(async move { dashboard.connect().await }).flat_map(|connected| {
            match connected {
                Ok(receiver) => UnboundedReceiverStream::new(receiver).left_stream(),
                Err(error) => {
                    tracing::error!(target: LOG, "Dashboard SSE initial snapshot failed: {error}");
                    futures::stream::empty().right_stream()
                }
            }
        });
    let frames = omni_server_kit::sse::with_ping_immediately(
        snapshots,
        SSE_HEARTBEAT,
        state.ctx.clock.clone(),
    );
    with_sse_headers(Sse::new(until_shutdown(frames, state.ctx.shutdown.clone())))
}
