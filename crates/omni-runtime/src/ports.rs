//! Cross-subsystem ports. Each port is set once during wiring and read
//! through its accessor; a consumer must handle an unset port (the providing
//! subsystem is disabled by configuration).
//!
//! Payloads that are DTOs owned by another package's `omni-api` module cross
//! as `serde_json::Value` produced by serializing that DTO; the consumer
//! deserializes into the same `omni-api` type, so this crate needs no
//! dependency on subsystem DTOs.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::email::{
    DownloadedAttachment, EmailAttachment, EmailHandler, EmailOrigin, FetchedEmail,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A port call failure.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("{0} is unavailable")]
    Unavailable(&'static str),
    #[error("{message}")]
    Failed { message: String, transient: bool },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailSearch {
    pub query: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub unread: Option<bool>,
    /// IMAP internal date lower bound (inclusive, day precision), epoch ms.
    pub since_ms: Option<i64>,
    /// IMAP internal date upper bound (exclusive, day precision), epoch ms.
    pub before_ms: Option<i64>,
    pub folder: Option<EmailFolderScope>,
    pub limit: u32,
    /// Bypass the search and parsed-message caches.
    pub fresh: bool,
    /// Messages whose `References` header names this Message-ID (a substring
    /// match on the server: callers check the parsed list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailFolderScope {
    Inbox,
    Archive,
    Sent,
    All,
}

/// Transport capabilities reported by `email_health`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmailReaderHealth {
    /// Short transport label (`"IMAP"`).
    pub transport: String,
    pub search_available: bool,
    pub drafts_available: bool,
}

/// Implemented by `omni-imap`; used by `omni-email` (retry, reprocess, MCP email
/// tools) and `omni-calendar` (attachment download).
pub trait EmailReader: Send + Sync {
    /// Re-fetches one email by stable id; `None` when it is gone.
    fn fetch_by_id<'a>(
        &'a self,
        id: &'a str,
        fresh: bool,
    ) -> BoxFuture<'a, Result<Option<FetchedEmail>, PortError>>;
    fn search<'a>(
        &'a self,
        q: &'a EmailSearch,
    ) -> BoxFuture<'a, Result<Vec<FetchedEmail>, PortError>>;
    fn health(&self) -> EmailReaderHealth;
    /// The attachment's bytes by its folder/UID
    /// handle; `None` when the message or part is no longer available.
    fn download_attachment<'a>(
        &'a self,
        attachment: &'a EmailAttachment,
    ) -> BoxFuture<'a, Result<Option<DownloadedAttachment>, PortError>>;
}

/// Implemented by `omni_imap::transport::StoreArchiveEcho`, which needs only
/// the store; used by `omni-mcp` to suppress mailbox events caused by its own
/// archive moves. Wiring must set it whenever MCP events are enabled: the MCP package fails
/// boot without it rather than publishing duplicate `email.received` events.
pub trait ArchiveEcho: Send + Sync {
    fn is_archive_action_message<'a>(
        &'a self,
        message_id: &'a str,
        origin: Option<&'a EmailOrigin>,
    ) -> BoxFuture<'a, Result<bool, PortError>>;
}

/// Implemented by app wiring; used by `omni-email` retry/reprocess to find a pipeline's handler.
pub trait EmailRetryHandlers: Send + Sync {
    fn handler(&self, pipeline: &str) -> Option<Arc<dyn EmailHandler>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarConnectionStatus {
    pub configured: bool,
    pub provider: Option<String>,
}

/// Implemented by `omni-calendar`; used by `omni-email` (the MCP health report).
pub trait CalendarConnection: Send + Sync {
    fn status(&self) -> CalendarConnectionStatus;
}

/// One source's copy of a public web page, unrated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchedPage {
    /// The retriever that produced it: `fetch` (the page itself), `wayback`
    /// (an archived snapshot), `obscura` (a browser render), or an extractor
    /// (`readability`, `postlight`, `extractus`, `jina`, `x`).
    pub source: String,
    /// Where this copy was read: the requested URL, the snapshot's URL for
    /// `wayback`, or where the browser ended up for `obscura`.
    pub url: String,
    /// The page's HTML when the source holds a whole page (`fetch`,
    /// `wayback`, `obscura`). Untrusted.
    pub html: Option<String>,
    pub title: Option<String>,
    /// Readable article text when the source extracted any. Untrusted.
    pub text: Option<String>,
    pub lead_image_url: Option<String>,
    pub author: Option<String>,
}

/// Implemented by `omni-presspods` over its article retrievers; used by
/// subsystems that read a public page a person submitted.
pub trait PageFetcher: Send + Sync {
    /// Every candidate the PressPods retrievers produce for a public
    /// `http(s)` URL, in retriever order and without the metadata model's
    /// rating. The retrievers run at once, each within 60 seconds, through
    /// the SSRF-guarded client. `Ok` holds at least one page; the failure
    /// message says whether the host refused, the host timed out or every
    /// source failed, and never carries a response body.
    fn fetch_pages<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<FetchedPage>, PortError>>;
}

/// Implemented by `omni-live`; used by `omni-live-intel`, `omni-mcp` and the
/// dashboard snapshot. Values are serialized
/// `omni_api::streamers` DTOs.
pub trait LiveDirectory: Send + Sync {
    /// Configured streamers, in configured order.
    fn streamers(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>>;
    /// Current aggregate statuses.
    fn statuses(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>>;
    /// Streamers serialized for display (dashboard snapshot and `/api/streamers`).
    fn display(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>>;
    fn details<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Value>, PortError>>;
}

/// An aggregate live-state edge for intelligence hooks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTransition {
    pub streamer_id: String,
    pub live: bool,
    pub at_ms: i64,
}

/// One streamer polled live this tick: sent for the
/// went-live edge and every still-live poll, never for a streamer whose poll
/// failed or was not due (background tier).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveObservation {
    /// A serialized `omni_api::streamers::LivestreamSummary`.
    pub streamer: Value,
    /// The new live status as a serialized `omni_api::streamers::StreamerStatusView`.
    pub status: Value,
    pub went_live: bool,
    pub title_changed: bool,
    pub at_ms: i64,
}

/// Implemented by `omni-live-intel`; used by `omni-live` (task hooks, routes)
/// and `omni-mcp`. Values are
/// serialized `omni_api::intelligence` DTOs.
pub trait LiveIntelligence: Send + Sync {
    /// A streamer polled live this tick.
    fn observe_live<'a>(&'a self, observation: &'a LiveObservation) -> BoxFuture<'a, ()>;
    /// Runs after every live-check tick.
    fn after_tick(&self) -> BoxFuture<'_, ()>;
    /// Aggregate edges; `live: false` is `observeOffline`.
    fn on_transition<'a>(&'a self, transition: &'a LiveTransition) -> BoxFuture<'a, ()>;
    fn details<'a>(
        &'a self,
        id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Option<Value>, PortError>>;
    fn diagnostics(&self) -> BoxFuture<'_, Result<Value, PortError>>;
    fn record_feedback(&self, input: Value) -> BoxFuture<'_, Result<Value, PortError>>;
}

/// Sample rate of the mono `f32` audio a [`SpeechTranscriber`] takes.
pub const SPEECH_SAMPLE_RATE: u32 = 16_000;

/// Recognized text of one window of a transcribed chunk. The offsets are
/// the audio the recognizer was given for it, in samples of the chunk: the
/// text was spoken somewhere inside, and no finer timing exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechSegment {
    pub start_sample: usize,
    pub end_sample: usize,
    pub text: String,
    /// The voice detector found speech here. `false` for text the recognizer
    /// heard in audio the detector took for silence, noise or music.
    pub detected: bool,
}

/// Speech the voice detector found but the recognizer returned no text for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechGap {
    pub start_sample: usize,
    pub end_sample: usize,
}

/// What the recognizer made of one audio chunk. Every consumed sample went
/// through the recognizer, whether or not the voice detector found speech.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChunkTranscript {
    pub segments: Vec<SpeechSegment>,
    /// Detected speech without text: known holes in the transcript.
    pub gaps: Vec<SpeechGap>,
    /// Samples of detected speech among the consumed ones.
    pub speech_samples: usize,
    /// Samples transcribed. Less than the chunk when it ended inside an
    /// utterance: the caller starts its next chunk here so no sentence is
    /// cut at a chunk boundary.
    pub consumed_samples: usize,
}

/// Implemented by `omni-live-intel` over its local speech models; used by
/// `omni-youtube` transcripts. Unset while livestream intelligence is disabled
/// or its models failed to load.
pub trait SpeechTranscriber: Send + Sync {
    /// The transcription model's name.
    fn model(&self) -> &'static str;
    /// Transcribes one chunk of [`SPEECH_SAMPLE_RATE`] mono audio, all of
    /// it: voice detection only chooses where to cut and what to report as
    /// a gap. `last` marks the end of the recording, where everything is
    /// consumed.
    fn transcribe_chunk(
        &self,
        samples: Arc<Vec<f32>>,
        last: bool,
    ) -> BoxFuture<'_, Result<ChunkTranscript, PortError>>;
}

/// Implemented by `omni-media`; used by the dashboard snapshot. Values are serialized
/// `omni_api::media::OnDeckItem`s.
pub trait OnDeckSource: Send + Sync {
    fn on_deck(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>>;
}

/// A turn Omni just started on the Claude Code host (`noteTurnStarted` input).
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeTurnStarted {
    pub session_id: String,
    pub id: Option<String>,
    pub project: Option<String>,
    /// The session revision the turn started from.
    pub revision: f64,
}

/// Implemented by the `omni-mcp` `claude.session.turn_finished` watcher; used by the
/// Claude session tools so a turn that ends before the next poll still
/// produces an event. Failures are the implementation's to log.
pub trait ClaudeSessionNotifier: Send + Sync {
    fn note_turn_started<'a>(&'a self, turn: &'a ClaudeTurnStarted) -> BoxFuture<'a, ()>;
}

/// The bounded commands of the Claude Code host's session client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostCommand {
    Projects,
    List,
    Status,
    Read,
    Result,
    Wait,
    Start,
    Send,
    Stop,
}

impl HostCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            HostCommand::Projects => "projects",
            HostCommand::List => "list",
            HostCommand::Status => "status",
            HostCommand::Read => "read",
            HostCommand::Result => "result",
            HostCommand::Wait => "wait",
            HostCommand::Start => "start",
            HostCommand::Send => "send",
            HostCommand::Stop => "stop",
        }
    }
}

/// `DeviceLinkStatus` of a configured link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostLinkStatus {
    pub online: bool,
    pub disabled: bool,
    /// The host's own name; never surfaced through MCP.
    pub host: Option<String>,
    pub last_seen_at: Option<String>,
    pub pending_jobs: usize,
}

/// A relay failure (`DeviceLinkError`).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{detail} ({code})")]
pub struct HostError {
    pub code: String,
    pub detail: String,
    pub retryable: bool,
}

impl HostError {
    pub fn new(code: impl Into<String>, detail: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            retryable,
        }
    }
}

/// The Claude Code host link: status plus bounded command execution.
/// Implemented by `omni_device_link::DeviceLinkService` (the outbound
/// long-poll relay); used by the `omni-mcp` Claude session tools, activity
/// routes and session watcher.
pub trait ClaudeHost: Send + Sync {
    fn status(&self) -> HostLinkStatus;
    /// Runs one command and returns the client's `data` payload.
    fn execute(
        &self,
        command: HostCommand,
        args: Map<String, Value>,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<Map<String, Value>, HostError>>;
}

/// One MCP Events source observation (`docs/mcp-events.md`).
#[derive(Clone, Debug, PartialEq)]
pub struct EventPublication {
    /// A catalog event name (`omni_api::events` or the MCP catalog).
    pub name: &'static str,
    /// Stable for the source observation: a replay with the same key is
    /// dropped and keeps the event ID. It is hashed before it is stored, so
    /// it may hold identifiers, but it must not change across versions.
    pub dedup_key: String,
    /// When the source observed it, epoch ms.
    pub occurred_at_ms: i64,
    /// The payload; it must match the event's payload schema.
    pub data: Map<String, Value>,
}

/// Implemented by `omni-mcp` over the durable MCP Events outbox; used by
/// subsystems that publish events. Unset when MCP Events are disabled, in
/// which case publishers skip publishing and any subscription-gated polling.
/// Call it outside the caller's own store transactions and locks.
pub trait EventPublisher: Send + Sync {
    /// Queues the event for every matching subscription, once per dedup key.
    /// `Ok(true)` when at least one delivery was queued. Holds no lock across
    /// webhook or authorization I/O.
    fn publish<'a>(&'a self, event: &'a EventPublication)
    -> BoxFuture<'a, Result<bool, PortError>>;
    /// Canonical arguments of every current subscription to `name`, so a
    /// publisher can poll only while subscribed or fan out per argument set.
    fn active_arguments<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Vec<BTreeMap<String, String>>, PortError>>;
    /// Queues one notice for a stored event target (a bot webhook) in the
    /// event journal, which delivers it with the journal's retries and
    /// records every attempt. At most once per `key`: a second call with the
    /// same key changes nothing. A target that does not exist yet holds the
    /// notice as `unconfigured` until it is created.
    fn notify_target<'a>(&'a self, notice: TargetNotice) -> BoxFuture<'a, Result<(), PortError>>;
    /// Where notices stand, by key; unknown keys are absent.
    fn notice_states<'a>(
        &'a self,
        keys: Vec<String>,
    ) -> BoxFuture<'a, Result<BTreeMap<String, NoticeState>, PortError>>;
    /// Drops a notice that has not gone out yet (its reason passed); `false`
    /// when it was already sent or settled.
    fn cancel_notice<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<bool, PortError>>;
}

/// One notice for [`EventPublisher::notify_target`].
#[derive(Clone, Debug, PartialEq)]
pub struct TargetNotice {
    /// The stored target's id (`kitchen`, `chief`, ...).
    pub target: String,
    /// The dedup key, also sent as `Idempotency-Key` on every attempt.
    pub key: String,
    /// The event name shown in diagnostics (`recipe.request`).
    pub name: String,
    /// The JSON body posted to the target.
    pub body: Value,
}

/// Where one notice stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeStatus {
    Pending,
    Delivered,
    /// Accepted, but the receiving bot ran no webhook routine.
    Ignored,
    /// Refused for good, or still failing a day later.
    Failed,
    /// The target did not exist when the notice was queued.
    Unconfigured,
    /// Cancelled before it went out.
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoticeState {
    pub status: NoticeStatus,
    pub attempts: u32,
}

/// Returned when a port is set twice.
#[derive(Debug, thiserror::Error)]
#[error("port {0} is already set")]
pub struct PortAlreadySet(pub &'static str);

macro_rules! ports {
    ($( $field:ident, $setter:ident : $trait:ident ),* $(,)?) => {
        #[derive(Default)]
        struct PortsInner {
            $( $field: OnceLock<Arc<dyn $trait>>, )*
        }

        /// All ports; cheap to clone, set once each during wiring.
        #[derive(Clone, Default)]
        pub struct Ports {
            inner: Arc<PortsInner>,
        }

        impl Ports {
            $(
                pub fn $field(&self) -> Option<Arc<dyn $trait>> {
                    self.inner.$field.get().cloned()
                }

                pub fn $setter(&self, port: Arc<dyn $trait>) -> Result<(), PortAlreadySet> {
                    self.inner
                        .$field
                        .set(port)
                        .map_err(|_| PortAlreadySet(stringify!($trait)))
                }
            )*
        }
    };
}

ports! {
    email_reader, set_email_reader: EmailReader,
    archive_echo, set_archive_echo: ArchiveEcho,
    email_retry_handlers, set_email_retry_handlers: EmailRetryHandlers,
    calendar_connection, set_calendar_connection: CalendarConnection,
    live_directory, set_live_directory: LiveDirectory,
    live_intelligence, set_live_intelligence: LiveIntelligence,
    speech_transcriber, set_speech_transcriber: SpeechTranscriber,
    page_fetcher, set_page_fetcher: PageFetcher,
    on_deck_source, set_on_deck_source: OnDeckSource,
    claude_session_notifier, set_claude_session_notifier: ClaudeSessionNotifier,
    claude_host, set_claude_host: ClaudeHost,
    event_publisher, set_event_publisher: EventPublisher,
}

impl Ports {
    /// Publishes through [`EventPublisher`] when MCP Events are enabled;
    /// `Ok(false)` when the port is unset.
    pub async fn publish_event(&self, event: &EventPublication) -> Result<bool, PortError> {
        match self.event_publisher() {
            Some(publisher) => publisher.publish(event).await,
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Deck;

    impl OnDeckSource for Deck {
        fn on_deck(&self) -> BoxFuture<'_, Result<Vec<Value>, PortError>> {
            Box::pin(async { Ok(vec![Value::Null]) })
        }
    }

    #[test]
    fn publishing_without_the_event_port_is_a_no_op() {
        use futures::FutureExt as _;
        let ports = Ports::default();
        let event = EventPublication {
            name: "task.run_finished",
            dedup_key: "run".to_owned(),
            occurred_at_ms: 0,
            data: Map::new(),
        };
        assert!(matches!(
            ports.publish_event(&event).now_or_never(),
            Some(Ok(false))
        ));
    }

    #[test]
    fn ports_are_set_once() {
        let ports = Ports::default();
        assert!(ports.on_deck_source().is_none());
        assert!(ports.set_on_deck_source(Arc::new(Deck)).is_ok());
        assert!(ports.clone().on_deck_source().is_some());
        assert!(ports.set_on_deck_source(Arc::new(Deck)).is_err());
    }
}
