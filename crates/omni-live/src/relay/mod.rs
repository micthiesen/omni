//! The Kick stream relay behind `/streamers/:id/watch`.
//!
//! The watch page starts one relay task per channel, which pulls the top
//! variant's segments from Kick into memory; the page plays Omni's own HLS
//! playlist of them. Segments are copied byte for byte (no transcode). The
//! buffer holds a cushion back from the player and releases at real-time pace
//! (`buffer`), so Kick stalls, token refreshes and playlist errors are absorbed
//! while the cushion lasts. A relay stops and frees its buffer once no player
//! has fetched from it for `idle_ms`.

pub mod buffer;
pub mod playlist;
pub mod source;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use omni_api::streamers::{StreamRelayState, StreamRelayStatus};
use omni_core::clock::SharedClock;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use buffer::{BufferConfig, BufferPhase, RelayBuffer};
use playlist::Variant;
use source::{KickPlaybackSource, Resolution, ResolvedPlayback};

const LOG: &str = "KickRelay";

/// A sequence this far below the last one pulled is a restarted stream, not a
/// lagging CDN node.
const RESTART_SEQUENCE_DROP: u64 = 30;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub buffer: BufferConfig,
    /// A relay with no player fetch for this long stops.
    pub idle_ms: i64,
    /// Before any player fetch, status polls keep a started relay alive this
    /// long (the page prebuffers while the viewer has not pressed Play).
    pub prebuffer_hold_ms: i64,
    /// The playback URL is re-resolved this often; Kick's token lasts ten
    /// minutes.
    pub refresh_ms: i64,
    /// Kick media playlist poll interval (segments are 2 s).
    pub poll_ms: u64,
    /// Lookup interval while Kick reports the channel offline.
    pub offline_poll_ms: u64,
    /// Channels relayed at once (each holds up to `buffer.max_bytes`).
    pub max_relays: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            buffer: BufferConfig::default(),
            idle_ms: 90_000,
            prebuffer_hold_ms: 5 * 60_000,
            refresh_ms: 7 * 60_000,
            poll_ms: 1_000,
            offline_poll_ms: 15_000,
            max_relays: 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceState {
    Resolving,
    Live,
    Offline,
    Failing(String),
}

struct RelayState {
    buffer: RelayBuffer,
    started_ms: i64,
    last_access_ms: i64,
    /// A player fetched the playlist or a segment.
    fetched: bool,
    source: SourceState,
    variant: Option<Variant>,
    refreshes: u32,
}

impl RelayState {
    /// A status read keeps the relay alive only while nothing plays yet:
    /// during startup, and for a bounded prebuffer hold before Play.
    fn hold_for_status(&mut self, now: i64, prebuffer_hold_ms: i64) {
        if self.buffer.phase() == BufferPhase::Filling
            || (!self.fetched && now - self.started_ms < prebuffer_hold_ms)
        {
            self.last_access_ms = now;
        }
    }
}

struct Relay {
    slug: String,
    generation: u64,
    cancel: CancellationToken,
    state: Mutex<RelayState>,
}

impl Relay {
    fn with<T>(&self, f: impl FnOnce(&mut RelayState) -> T) -> T {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }
}

struct Inner {
    source: KickPlaybackSource,
    clock: SharedClock,
    tracker: TaskTracker,
    shutdown: CancellationToken,
    config: RelayConfig,
    /// Lock order: `relays`, then a relay's `state`.
    relays: Mutex<HashMap<String, Arc<Relay>>>,
    generations: AtomicU64,
}

impl Inner {
    fn relays(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Relay>>> {
        self.relays
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Removes the relay if it is idle (or `force`), under the registry lock
    /// so no status read can hand out a generation that is about to go.
    fn remove_if(&self, relay: &Arc<Relay>, now: i64, force: bool) -> bool {
        let mut relays = self.relays();
        let idle = force || now - relay.with(|s| s.last_access_ms) > self.config.idle_ms;
        if idle
            && relays
                .get(&relay.slug)
                .is_some_and(|current| Arc::ptr_eq(current, relay))
        {
            relays.remove(&relay.slug);
        }
        idle
    }
}

/// A served playlist request's outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum PlaylistLookup {
    Ready(String),
    /// The relay is running but has released nothing yet.
    NotReady,
    /// No relay of that generation (stopped, or replaced).
    Gone,
}

/// Every running relay, one per Kick channel.
#[derive(Clone)]
pub struct KickRelays {
    inner: Arc<Inner>,
}

impl KickRelays {
    pub fn new(
        source: KickPlaybackSource,
        clock: SharedClock,
        tracker: TaskTracker,
        shutdown: CancellationToken,
        config: RelayConfig,
    ) -> Self {
        // Generations start at boot time so a player URL from before a
        // restart is gone, not silently another stream's.
        let first = u64::try_from(clock.now_ms()).unwrap_or(1);
        Self {
            inner: Arc::new(Inner {
                source,
                clock,
                tracker,
                shutdown,
                config,
                relays: Mutex::new(HashMap::new()),
                generations: AtomicU64::new(first),
            }),
        }
    }

    /// Channels with a running relay.
    pub fn running(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self.inner.relays().keys().cloned().collect();
        slugs.sort();
        slugs
    }

    /// The channel's relay status; `Stopped` when none runs. Never starts one.
    /// `playlist_url` builds the served playlist path for a generation.
    pub fn status(&self, slug: &str, playlist_url: impl Fn(u64) -> String) -> StreamRelayStatus {
        let now = self.inner.clock.now_ms();
        let relays = self.inner.relays();
        let Some(relay) = relays.get(slug) else {
            return self.idle_status(StreamRelayState::Stopped, None);
        };
        let url = playlist_url(relay.generation);
        relay.with(|state| {
            state.buffer.advance(now);
            state.hold_for_status(now, self.inner.config.prebuffer_hold_ms);
            status_of(state, url)
        })
    }

    /// Starts the channel's relay when none runs, and returns its status.
    pub fn start(&self, slug: &str, playlist_url: impl Fn(u64) -> String) -> StreamRelayStatus {
        let now = self.inner.clock.now_ms();
        let mut relays = self.inner.relays();
        let relay = match relays.get(slug) {
            Some(relay) => relay.clone(),
            None => {
                if relays.len() >= self.inner.config.max_relays {
                    let message = format!(
                        "{} other channels are already relaying",
                        self.inner.config.max_relays
                    );
                    return self.idle_status(StreamRelayState::Error, Some(message));
                }
                let relay = Arc::new(Relay {
                    slug: slug.to_owned(),
                    generation: self.inner.generations.fetch_add(1, Ordering::Relaxed),
                    cancel: self.inner.shutdown.child_token(),
                    state: Mutex::new(RelayState {
                        buffer: RelayBuffer::new(self.inner.config.buffer.clone()),
                        started_ms: now,
                        last_access_ms: now,
                        fetched: false,
                        source: SourceState::Resolving,
                        variant: None,
                        refreshes: 0,
                    }),
                });
                relays.insert(slug.to_owned(), relay.clone());
                omni_core::spawn::spawn_tracked(
                    &self.inner.tracker,
                    "kick-relay",
                    run(self.inner.clone(), relay.clone()),
                );
                relay
            }
        };
        let url = playlist_url(relay.generation);
        relay.with(|state| {
            state.buffer.advance(now);
            state.last_access_ms = now;
            status_of(state, url)
        })
    }

    fn idle_status(&self, state: StreamRelayState, message: Option<String>) -> StreamRelayStatus {
        StreamRelayStatus {
            state,
            playlist_url: None,
            cushion_ms: 0,
            target_cushion_ms: self.inner.config.buffer.cushion_ms,
            startup_remaining_ms: None,
            resolution: None,
            frame_rate: None,
            bandwidth: None,
            stalls: 0,
            refreshes: 0,
            discontinuities: 0,
            buffered_bytes: 0,
            message,
        }
    }

    fn touched(&self, slug: &str, generation: u64) -> Option<Arc<Relay>> {
        let now = self.inner.clock.now_ms();
        let relays = self.inner.relays();
        let relay = relays.get(slug).filter(|r| r.generation == generation)?;
        relay.with(|state| {
            state.last_access_ms = now;
            state.fetched = true;
            state.buffer.advance(now);
        });
        Some(relay.clone())
    }

    /// The served media playlist.
    pub fn playlist(&self, slug: &str, generation: u64) -> PlaylistLookup {
        let Some(relay) = self.touched(slug, generation) else {
            return PlaylistLookup::Gone;
        };
        relay.with(|state| match state.buffer.render() {
            Some(text) => PlaylistLookup::Ready(text),
            None => PlaylistLookup::NotReady,
        })
    }

    /// A released segment's bytes.
    pub fn segment(&self, slug: &str, generation: u64, sequence: u64) -> Option<Bytes> {
        self.touched(slug, generation)?
            .with(|state| state.buffer.segment(sequence))
    }
}

fn status_of(state: &RelayState, playlist_url: String) -> StreamRelayStatus {
    let buffer = &state.buffer;
    let phase = buffer.phase();
    let (relay_state, message) = match (&state.source, phase) {
        (SourceState::Offline, BufferPhase::Playing) => (
            StreamRelayState::Playing,
            Some("Kick reports the channel offline; playing what is buffered".to_owned()),
        ),
        (SourceState::Offline, _) => (
            StreamRelayState::Offline,
            Some("Kick reports the channel offline".to_owned()),
        ),
        (_, BufferPhase::Playing) => (StreamRelayState::Playing, None),
        (SourceState::Failing(reason), _) => (StreamRelayState::Error, Some(reason.clone())),
        (_, BufferPhase::Filling) => (StreamRelayState::Starting, None),
        (_, BufferPhase::Rebuffering) => (StreamRelayState::Rebuffering, None),
    };
    StreamRelayStatus {
        state: relay_state,
        playlist_url: (phase != BufferPhase::Filling).then_some(playlist_url),
        cushion_ms: buffer.cushion_ms(),
        target_cushion_ms: buffer.config().cushion_ms,
        startup_remaining_ms: buffer.startup_remaining_ms(),
        resolution: state.variant.as_ref().and_then(Variant::resolution),
        frame_rate: state.variant.as_ref().and_then(|v| v.frame_rate),
        bandwidth: state.variant.as_ref().map(|v| v.bandwidth),
        stalls: buffer.stalls(),
        refreshes: state.refreshes,
        discontinuities: buffer.discontinuities(),
        buffered_bytes: buffer.bytes() as u64,
        message,
    }
}

/// Pull-side state of one relay task.
#[derive(Default)]
struct Pull {
    current: Option<ResolvedPlayback>,
    resolved_at: i64,
    /// Resolve before the next playlist read.
    force: bool,
    /// The last segment pushed: (livestream id, Kick sequence).
    last: Option<(u64, u64)>,
    /// Consecutive failed lookups.
    failures: u32,
    /// Consecutive expired, moved or ended answers since a segment last
    /// arrived; repeats back off instead of re-resolving in a tight loop.
    expired: u32,
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((500u64 << attempt.min(5)).min(15_000))
}

/// Re-resolve: at once the first time, with backoff on repeats.
fn expired(pull: &mut Pull) -> Duration {
    pull.force = true;
    pull.expired += 1;
    if pull.expired == 1 {
        Duration::ZERO
    } else {
        backoff(pull.expired)
    }
}

async fn run(inner: Arc<Inner>, relay: Arc<Relay>) {
    tracing::info!(target: LOG, channel = %relay.slug, "Relay started");
    let mut pull = Pull::default();
    loop {
        let now = inner.clock.now_ms();
        if inner.remove_if(&relay, now, relay.cancel.is_cancelled()) {
            break;
        }
        let wait = pull_once(&inner, &relay, &mut pull, now).await;
        relay.with(|s| s.buffer.advance(inner.clock.now_ms()));
        tokio::select! {
            () = relay.cancel.cancelled() => {
                inner.remove_if(&relay, now, true);
                break;
            }
            () = tokio::time::sleep(wait) => {}
        }
    }
    let held = relay.with(|s| {
        let held = s.buffer.bytes();
        s.buffer.clear();
        held
    });
    tracing::info!(
        target: LOG,
        channel = %relay.slug,
        freed_bytes = held,
        "Relay stopped"
    );
}

/// One resolve-if-due, playlist read and segment download pass. Returns how
/// long to wait before the next.
async fn pull_once(inner: &Inner, relay: &Relay, pull: &mut Pull, now: i64) -> Duration {
    let config = &inner.config;
    let poll = Duration::from_millis(config.poll_ms);
    if pull.force || pull.current.is_none() || now - pull.resolved_at >= config.refresh_ms {
        match inner.source.resolve(&relay.slug).await {
            Ok(Resolution::Live(playback)) => {
                let refreshed = pull.current.is_some();
                if let Some((id, _)) = pull.last
                    && id != playback.livestream_id
                {
                    relay.with(|s| s.buffer.mark_discontinuity());
                    pull.last = None;
                }
                relay.with(|s| {
                    s.refreshes += u32::from(refreshed);
                    s.variant = Some(playback.variant.clone());
                    s.source = SourceState::Live;
                });
                pull.current = Some(playback);
                pull.resolved_at = now;
                pull.force = false;
                pull.failures = 0;
            }
            Ok(Resolution::Offline) => {
                relay.with(|s| s.source = SourceState::Offline);
                pull.current = None;
                pull.force = false;
                pull.failures = 0;
                return Duration::from_millis(config.offline_poll_ms);
            }
            Err(error) => {
                pull.failures += 1;
                let wait = backoff(pull.failures);
                if pull.failures == 3 {
                    tracing::warn!(target: LOG, channel = %relay.slug, "Kick playback lookup failing: {error}");
                }
                if pull.current.is_none() {
                    relay.with(|s| s.source = SourceState::Failing(error.to_string()));
                    return wait;
                }
                // The playlist resolved earlier may still work: read it and
                // retry the lookup after the backoff.
                pull.force = false;
                #[allow(clippy::cast_possible_truncation)]
                let wait_ms = wait.as_millis() as i64;
                pull.resolved_at = now - config.refresh_ms + wait_ms;
            }
        }
    }
    let Some(current) = pull.current.clone() else {
        return poll;
    };
    let playlist = match inner.source.media_playlist(&current.variant.url).await {
        Ok(playlist) => playlist,
        Err(error) => {
            if error.needs_resolve() {
                return expired(pull);
            }
            tracing::debug!(target: LOG, channel = %relay.slug, "{error}");
            return poll;
        }
    };
    let mut pushed = false;
    for segment in playlist.segments {
        if let Some((id, last)) = pull.last
            && id == current.livestream_id
        {
            if segment.sequence <= last {
                if last - segment.sequence <= RESTART_SEQUENCE_DROP {
                    continue;
                }
                relay.with(|s| s.buffer.mark_discontinuity());
                pull.last = None;
            } else if segment.sequence > last + 1 {
                relay.with(|s| s.buffer.mark_discontinuity());
            }
        }
        match inner.source.segment(&segment.url).await {
            Ok(bytes) => {
                relay.with(|s| {
                    s.buffer
                        .push(segment.duration_ms, bytes, segment.discontinuity);
                    s.buffer.advance(inner.clock.now_ms());
                });
                pull.last = Some((current.livestream_id, segment.sequence));
                pull.expired = 0;
                pushed = true;
            }
            Err(error) if error.needs_resolve() && pull.expired > 0 => {
                // Still refused after a fresh resolve: the segment aged out
                // or its edge is broken. Skip it rather than stall on it.
                tracing::debug!(target: LOG, channel = %relay.slug, "Skipping a segment: {error}");
                relay.with(|s| s.buffer.mark_discontinuity());
                pull.last = Some((current.livestream_id, segment.sequence));
            }
            Err(error) => {
                if error.needs_resolve() {
                    return expired(pull);
                }
                tracing::debug!(target: LOG, channel = %relay.slug, "{error}");
                return poll;
            }
        }
        if relay.cancel.is_cancelled() {
            break;
        }
    }
    if playlist.ended {
        if !pushed {
            return expired(pull).max(poll);
        }
        pull.force = true;
    }
    poll
}
