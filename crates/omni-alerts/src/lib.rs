//! Pushover delivery and ERROR-log alerts.
//!
//! Every send first offers the message to the optional [`AppPushRoute`] (the
//! iOS app over APNs, set once at wiring). Pushover is the fallback only when
//! the route reports [`AppDelivery::NotRouted`]; an uncertain app delivery is
//! an error with no status, which callers treat like an uncertain Pushover
//! send and never resend.
//!
//! Notification paths are throttled at exactly one layer (AGENTS.md): the
//! [`throttle`] here applies only to ERROR-log alerts; feature notifications
//! own their deduplication.

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use futures::future::BoxFuture;
use omni_config::Config;
use omni_core::clock::SharedClock;
use omni_http::{HttpClient, Method, SideEffectMode, Url};
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;

pub use omni_config::PushoverChannel;

pub mod task_gate;
pub mod throttle;

pub use task_gate::TaskFailureGate;

/// Log target for this crate's own diagnostics (never logged at ERROR, so the
/// alert path cannot feed itself).
const LOG: &str = "Alerts";

/// The Pushover messages endpoint.
pub const PUSHOVER_API_URL: &str = "https://api.pushover.net/1/messages.json";
/// A hung request would otherwise hold up the alert worker and shutdown.
const PUSHOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Pushover responses are tiny; anything larger is cut off.
const PUSHOVER_MAX_RESPONSE: usize = 64 * 1024;

/// One Pushover message (`api.pushover.net/1/messages.json` fields).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushoverMessage {
    pub message: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub url_title: Option<String>,
    pub priority: Option<i8>,
    pub sound: Option<String>,
    pub timestamp: Option<i64>,
    /// Groups related app notifications (`aps.thread-id`); the channel name
    /// when unset. Pushover ignores it.
    pub thread_id: Option<String>,
    /// The person or streamer a notification is about, shown as a
    /// communication notification by the app. Pushover ignores it.
    pub sender: Option<NotificationSender>,
    /// App widget kinds to reload when the notification arrives (for example
    /// `"on-air"`). Pushover ignores it.
    pub refresh_widgets: Vec<String>,
}

/// The sender an app notification is about (communication notification style).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationSender {
    /// A stable id (the streamer id).
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Sent,
    SkippedNoToken,
    /// No `PUSHOVER_USER` configured.
    Disabled,
    /// `SideEffectMode::Record`: captured, not sent.
    Recorded,
}

/// The API rejected the message (`status`, with the response `body`), or the
/// request never completed (`status: None`, `body` is the cause's message).
/// The display text is what run errors persist and the UI shows.
#[derive(thiserror::Error, Debug)]
pub struct PushoverError {
    pub status: Option<u16>,
    pub body: String,
}

impl std::fmt::Display for PushoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(
                f,
                "Pushover API returned status code {status}: {}",
                self.body
            ),
            None => write!(f, "Pushover request failed: {}", self.body),
        }
    }
}

impl PushoverError {
    /// A 4xx response: retrying the same request will not succeed.
    pub fn is_definite_rejection(&self) -> bool {
        self.status
            .is_some_and(|status| (400..500).contains(&status))
    }
}

/// What the app route did with one message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppDelivery {
    /// The app is not set up to receive it (no alert device, APNs not
    /// configured, or every device definitively rejected it): use Pushover.
    NotRouted,
    /// At least one device accepted it.
    Delivered,
    /// No device accepted it and at least one attempt has an unknown outcome;
    /// never fall back or resend.
    Uncertain(String),
}

/// Delivers notifications to the Omni app before Pushover is considered.
///
/// Implementations must never log at ERROR: ERROR-log alerts are delivered
/// through this route, so an ERROR here would feed itself.
pub trait AppPushRoute: Send + Sync {
    fn deliver<'a>(
        &'a self,
        channel: PushoverChannel,
        message: &'a PushoverMessage,
    ) -> BoxFuture<'a, AppDelivery>;
}

/// The route was set twice.
#[derive(Debug, thiserror::Error)]
#[error("the app push route is already set")]
pub struct AppRouteAlreadySet;

/// Pushover client with the configured user and per-channel tokens.
#[derive(Clone)]
pub struct Pushover {
    inner: Arc<PushoverInner>,
}

struct PushoverInner {
    app_route: OnceLock<Arc<dyn AppPushRoute>>,
    http: HttpClient,
    user: Option<String>,
    tokens: Vec<(PushoverChannel, Option<String>)>,
    mode: SideEffectMode,
    recorded: Mutex<Vec<RecordedPush>>,
}

/// A message captured in `SideEffectMode::Record`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPush {
    pub token: String,
    pub message: PushoverMessage,
}

/// Every channel, for building token tables.
const CHANNELS: [PushoverChannel; 6] = [
    PushoverChannel::General,
    PushoverChannel::Live,
    PushoverChannel::Calendar,
    PushoverChannel::Recs,
    PushoverChannel::Podcast,
    PushoverChannel::PressPods,
];

impl Pushover {
    pub fn new(http: HttpClient, config: &Config, mode: SideEffectMode) -> Self {
        let tokens = CHANNELS
            .into_iter()
            .map(|ch| (ch, config.pushover_token(ch).map(str::to_owned)))
            .collect();
        Self::from_parts(http, config.pushover_user.clone(), tokens, mode)
    }

    /// Explicit credentials: `user` plus a token per channel (already
    /// resolved, including the `PUSHOVER_TOKEN` fallback).
    pub fn with_credentials(
        http: HttpClient,
        user: Option<String>,
        tokens: impl IntoIterator<Item = (PushoverChannel, String)>,
        mode: SideEffectMode,
    ) -> Self {
        let given: Vec<(PushoverChannel, String)> = tokens.into_iter().collect();
        let tokens = CHANNELS
            .into_iter()
            .map(|ch| {
                let token = given
                    .iter()
                    .find(|(channel, _)| *channel == ch)
                    .map(|(_, token)| token.clone());
                (ch, token)
            })
            .collect();
        Self::from_parts(http, user, tokens, mode)
    }

    fn from_parts(
        http: HttpClient,
        user: Option<String>,
        tokens: Vec<(PushoverChannel, Option<String>)>,
        mode: SideEffectMode,
    ) -> Self {
        Self {
            inner: Arc::new(PushoverInner {
                app_route: OnceLock::new(),
                http,
                user,
                tokens,
                mode,
                recorded: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Sets the app route shared by every clone of this client (once, at wiring).
    pub fn set_app_route(&self, route: Arc<dyn AppPushRoute>) -> Result<(), AppRouteAlreadySet> {
        self.inner
            .app_route
            .set(route)
            .map_err(|_| AppRouteAlreadySet)
    }

    /// Whether an app route is set (it may still route nothing).
    pub fn has_app_route(&self) -> bool {
        self.inner.app_route.get().is_some()
    }

    /// Offers `m` to the app route: `Some` when it settled the message.
    async fn try_app(
        &self,
        ch: PushoverChannel,
        m: &PushoverMessage,
    ) -> Option<Result<PushOutcome, PushoverError>> {
        let route = self.inner.app_route.get()?;
        match route.deliver(ch, m).await {
            AppDelivery::NotRouted => None,
            AppDelivery::Delivered => Some(Ok(PushOutcome::Sent)),
            AppDelivery::Uncertain(detail) => Some(Err(PushoverError {
                status: None,
                body: format!("app push outcome unknown: {detail}"),
            })),
        }
    }

    /// Sends on a channel: the app first, then Pushover (10 s timeout).
    pub async fn send(
        &self,
        ch: PushoverChannel,
        m: PushoverMessage,
    ) -> Result<PushOutcome, PushoverError> {
        if let Some(settled) = self.try_app(ch, &m).await {
            return settled;
        }
        self.send_pushover(ch, m).await
    }

    async fn send_pushover(
        &self,
        ch: PushoverChannel,
        m: PushoverMessage,
    ) -> Result<PushOutcome, PushoverError> {
        let token = self
            .inner
            .tokens
            .iter()
            .find(|(channel, _)| *channel == ch)
            .and_then(|(_, token)| token.clone());
        match token {
            Some(token) => self.send_token(&token, m).await,
            None => Ok(PushOutcome::SkippedNoToken),
        }
    }

    /// Sends with an explicit application token (per-streamer Pushover apps):
    /// the app first, on the Live channel, then Pushover.
    pub async fn send_with_token(
        &self,
        token: &str,
        m: PushoverMessage,
    ) -> Result<PushOutcome, PushoverError> {
        if let Some(settled) = self.try_app(PushoverChannel::Live, &m).await {
            return settled;
        }
        self.send_token(token, m).await
    }

    async fn send_token(
        &self,
        token: &str,
        m: PushoverMessage,
    ) -> Result<PushOutcome, PushoverError> {
        let inner = &self.inner;
        if inner.user.as_deref().is_none_or(str::is_empty) {
            return Ok(PushOutcome::Disabled);
        }
        if inner.mode == SideEffectMode::Record {
            inner
                .recorded
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(RecordedPush {
                    token: token.to_owned(),
                    message: m,
                });
            return Ok(PushOutcome::Recorded);
        }
        if token.is_empty() {
            tracing::debug!(target: LOG, "Pushover message skipped: no token configured or given");
            return Ok(PushOutcome::SkippedNoToken);
        }
        let user = inner.user.as_deref().unwrap_or_default();
        let form = pushover_form(token, user, &m);
        let fields: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let url = Url::parse(PUSHOVER_API_URL).map_err(|e| PushoverError {
            status: None,
            body: e.to_string(),
        })?;
        let response = inner
            .http
            .request(Method::POST, url)
            .form(&fields)
            .timeout(PUSHOVER_TIMEOUT)
            .send_bounded(PUSHOVER_MAX_RESPONSE)
            .await
            .map_err(|error| match &error {
                // An HTTP status error keeps its status and no body.
                omni_http::HttpError::Status { status, .. } => PushoverError {
                    status: Some(*status),
                    body: String::new(),
                },
                _ => PushoverError {
                    status: None,
                    body: error.to_string(),
                },
            })?;
        if !response.status.is_success() {
            return Err(PushoverError {
                status: Some(response.status.as_u16()),
                body: String::from_utf8_lossy(&response.body).into_owned(),
            });
        }
        Ok(PushOutcome::Sent)
    }

    /// Whether a `PUSHOVER_USER` is configured, so messages
    /// with a token are sent (or recorded).
    pub fn is_configured(&self) -> bool {
        self.inner
            .user
            .as_deref()
            .is_some_and(|user| !user.is_empty())
    }

    /// Whether `ch` resolves to a non-empty token (its own or `PUSHOVER_TOKEN`).
    pub fn has_token(&self, ch: PushoverChannel) -> bool {
        self.inner.tokens.iter().any(|(channel, token)| {
            *channel == ch && token.as_deref().is_some_and(|t| !t.is_empty())
        })
    }

    /// Messages captured in `SideEffectMode::Record`, oldest first.
    pub fn recorded(&self) -> Vec<RecordedPush> {
        self.inner
            .recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The Pushover form fields: empty title, url,
/// url title and sound and a zero timestamp are omitted.
fn pushover_form(token: &str, user: &str, m: &PushoverMessage) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("token", token.to_owned()),
        ("user", user.to_owned()),
        ("message", m.message.clone()),
    ];
    let non_empty = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());
    if let Some(title) = non_empty(&m.title) {
        form.push(("title", title));
    }
    if let Some(url) = non_empty(&m.url) {
        form.push(("url", url));
    }
    if let Some(url_title) = non_empty(&m.url_title) {
        form.push(("url_title", url_title));
    }
    if let Some(priority) = m.priority {
        form.push(("priority", priority.to_string()));
    }
    if let Some(sound) = non_empty(&m.sound) {
        form.push(("sound", sound));
    }
    if let Some(timestamp) = m.timestamp.filter(|t| *t != 0) {
        form.push(("timestamp", timestamp.to_string()));
    }
    form
}

/// A notification gate consulted before the throttle; the first gate that
/// applies to a title decides it.
pub trait AlertGate: Send + Sync {
    fn applies(&self, title: &str) -> bool;
    fn should_notify<'a>(&'a self, title: &'a str) -> BoxFuture<'a, bool>;
}

/// An ERROR log line headed for Pushover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertNotification {
    pub logger_name: String,
    /// The log message.
    pub title: String,
    /// Formatted fields, or the message itself when there are none.
    pub body: String,
}

/// Queue depth between the layer and the worker; overflow is dropped.
const ALERT_QUEUE: usize = 256;

/// Tracing layer: every ERROR event becomes an [`AlertNotification`].
pub struct AlertLayer {
    tx: mpsc::Sender<AlertNotification>,
}

impl AlertLayer {
    /// The layer plus the worker that drains it (gates, throttle, Pushover General).
    pub fn new(
        p: Pushover,
        gates: Arc<RwLock<Vec<Arc<dyn AlertGate>>>>,
        clock: SharedClock,
    ) -> (Self, AlertWorker) {
        let (tx, rx) = mpsc::channel(ALERT_QUEUE);
        (
            Self { tx },
            AlertWorker {
                rx,
                pushover: p,
                gates,
                clock,
                throttle: throttle::AlertThrottle::default(),
            },
        )
    }
}

#[derive(Default)]
struct AlertVisitor {
    message: String,
    fields: Vec<String>,
}

impl Visit for AlertVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message.push_str(&format!("{value:?}"));
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }
}

/// Application ERROR logs only: a named logger target (`"Scheduler"`,
/// `"Main:TaskRegistry"`) or an `omni_*` or `omni::` module path, never
/// dependency internals.
fn is_app_error(metadata: &tracing::Metadata<'_>) -> bool {
    let target = metadata.target();
    *metadata.level() == Level::ERROR
        && (!target.contains("::") || target.starts_with("omni_") || target.starts_with("omni::"))
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for AlertLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !is_app_error(event.metadata()) {
            return;
        }
        let mut visitor = AlertVisitor::default();
        event.record(&mut visitor);
        let body = if visitor.fields.is_empty() {
            visitor.message.clone()
        } else {
            visitor.fields.join(" ")
        };
        let notification = AlertNotification {
            logger_name: event.metadata().target().to_owned(),
            title: visitor.message,
            body,
        };
        // A full queue drops the alert: logging must never block.
        let _ = self.tx.try_send(notification);
    }
}

/// Drains alert notifications; run it on the app tracker.
pub struct AlertWorker {
    rx: mpsc::Receiver<AlertNotification>,
    pushover: Pushover,
    gates: Arc<RwLock<Vec<Arc<dyn AlertGate>>>>,
    clock: SharedClock,
    throttle: throttle::AlertThrottle,
}

impl AlertWorker {
    /// Runs until every [`AlertLayer`] sender is dropped.
    pub async fn run(mut self) {
        while let Some(alert) = self.rx.recv().await {
            self.handle(alert).await;
        }
    }

    async fn handle(&mut self, alert: AlertNotification) {
        let gate = {
            let gates = self
                .gates
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            gates
                .iter()
                .find(|gate| gate.applies(&alert.title))
                .cloned()
        };
        if let Some(gate) = gate
            && !gate.should_notify(&alert.title).await
        {
            return;
        }
        let key = throttle::alert_key(&alert.logger_name, &alert.title);
        let Some(admitted) = self.throttle.admit(
            throttle::ThrottledAlert {
                key,
                title: alert.title,
                body: alert.body,
            },
            self.clock.now_ms(),
        ) else {
            return;
        };
        let message = PushoverMessage {
            title: Some(format!("Error: {}", admitted.title)),
            message: admitted.body,
            ..PushoverMessage::default()
        };
        if let Err(error) = self.pushover.send(PushoverChannel::General, message).await {
            tracing::warn!(target: LOG, %error, "Failed to deliver error alert");
        }
    }
}
