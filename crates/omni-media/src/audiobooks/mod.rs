//! Audiobooks from MyAnonamouse into the Plex audiobook library that
//! Prologue reads, end to end.
//!
//! - Search: Prowlarr's audiobook category on one indexer
//!   (`AUDIOBOOK_INDEXER`), ranked by [`rank`].
//! - Grab: a fresh search finds the chosen release again, Omni downloads its
//!   `.torrent` through Prowlarr, reads the info hash, reserves the grab
//!   durably by that hash, and adds the torrent to Deluge's daemon under
//!   `DELUGE_LABEL`. The label must move finished torrents into a folder that
//!   is one of the Plex library's locations, so Plex only ever sees complete
//!   downloads. The add is verified by reading the torrent back; an add whose
//!   outcome is unknown is settled only by reading, never by adding again.
//! - Watch ([`watch`]): progress, stalls, a finished download without audio,
//!   a targeted Plex scan, and the match of the torrent's files to a Plex
//!   album; then one "ready" notification. Problems notify once each.
//! - Manage: status and cancel (before the download finishes only; a
//!   finished torrent keeps seeding, which MyAnonamouse requires).
//!
//! Omni only touches torrents it added. Release guids and download URLs carry
//! passkeys and API keys; they never leave this module (releases are named by
//! an opaque `releaseId`, grabs by the public info hash), and titles pass
//! `inspect::redact`.

pub mod deluge;
pub mod pending;
pub mod rank;
pub mod tagging;
pub mod torrent;
pub mod watch;

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use omni_alerts::Pushover;
use omni_core::spawn::must_complete;
use omni_http::{HttpClient, Method, SideEffectMode, Url};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{DocOps as _, Store, StoreError};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::task::TaskTracker;

use self::deluge::{Added, TorrentClient, TorrentStatus};
use self::rank::{Assessment, Candidate, words};
use crate::arr::inspect::redact;
use crate::plex::{MusicSection, PlexClient};

const LOG: &str = "Audiobooks";
const HOUR_MS: i64 = 60 * 60 * 1000;
/// Grab rows are kept this long after their last change.
const GRAB_RECORD_TTL_MS: i64 = 180 * 24 * HOUR_MS;
/// Newznab's Audio/Audiobook category.
const AUDIOBOOK_CATEGORY: &str = "3030";
/// Prowlarr waits for the indexer.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(90);
const SEARCH_MAX_BYTES: usize = 16 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const READ_MAX_BYTES: usize = 4 * 1024 * 1024;
const TORRENT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// The most releases one search returns.
pub const RELEASE_LIMIT: usize = 20;
/// The most grabs one status call returns.
const STATUS_LIMIT: usize = 30;
/// Audio a finished download must contain to count as an audiobook.
const AUDIO_EXTENSIONS: [&str; 8] = ["m4b", "m4a", "aac", "mp3", "flac", "ogg", "opus", "wma"];

pub(crate) fn addition_id(id: &str) -> String {
    omni_core::digest::sha256_hex(format!("library-addition\n{id}"))
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProwlarrRelease {
    #[serde(default)]
    guid: Option<String>,
    #[serde(default)]
    indexer_id: Option<i64>,
    #[serde(default)]
    indexer: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    files: Option<i64>,
    #[serde(default)]
    seeders: Option<i64>,
    #[serde(default)]
    leechers: Option<i64>,
    #[serde(default)]
    grabs: Option<i64>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    publish_date: Option<String>,
    #[serde(default)]
    download_url: Option<String>,
    #[serde(default)]
    indexer_flags: Vec<String>,
}

impl ProwlarrRelease {
    /// An opaque handle: the guid can be a download URL carrying a passkey.
    fn release_id(&self) -> Option<String> {
        let digest = omni_core::digest::sha256_hex(format!(
            "{}\n{}",
            self.indexer_id?,
            self.guid.as_deref()?
        ));
        Some(digest.get(..32).unwrap_or(&digest).to_owned())
    }

    fn freeleech(&self) -> bool {
        self.indexer_flags
            .iter()
            .any(|f| f.to_ascii_lowercase().contains("freeleech"))
    }
}

/// Decodes each release on its own, so one malformed entry drops only itself.
fn lenient_releases(raw: Vec<Value>) -> Vec<ProwlarrRelease> {
    raw.into_iter()
        .filter_map(|r| serde_json::from_value(r).ok())
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexerResource {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enable: bool,
}

/// A torrent release, ranked.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookRelease {
    pub rank: usize,
    pub release_id: String,
    pub title: String,
    pub book: Option<String>,
    pub author: Option<String>,
    /// The indexer's language code (ENG, DUT, ...).
    pub language: Option<String>,
    /// Best first.
    pub formats: Vec<String>,
    pub size_mb: Option<i64>,
    pub files: Option<i64>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    pub grabs: Option<i64>,
    pub freeleech: bool,
    pub published_at: Option<String>,
    /// Matches the query, seeded, preferred language, unabridged, not VIP.
    pub good_match: bool,
    /// Why it ranks where it does.
    pub notes: Vec<String>,
}

impl AudiobookRelease {
    fn of(rank: usize, release: &ProwlarrRelease, a: Assessment) -> Option<Self> {
        #[allow(clippy::cast_possible_truncation)]
        let size_mb = release
            .size
            .map(|bytes| (bytes / 1_000_000.0).round() as i64);
        Some(Self {
            rank,
            release_id: release.release_id()?,
            title: redact(release.title.as_deref()?),
            book: a.book,
            author: a.author,
            language: a.language,
            formats: a.formats,
            size_mb,
            files: release.files,
            seeders: release.seeders,
            leechers: release.leechers,
            grabs: release.grabs,
            freeleech: release.freeleech(),
            published_at: release.publish_date.clone(),
            good_match: a.good_match,
            notes: a.notes,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchResult {
    Listed,
    Unavailable,
}

/// Something matching the query that is already in the library or on its way.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlreadyHave {
    pub title: String,
    #[serde(skip)]
    pub safe_title: Option<String>,
    pub author: Option<String>,
    /// in_library, or a grab's stage.
    pub status: String,
    pub grab_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookSearch {
    pub result: SearchResult,
    pub releases: Vec<AudiobookRelease>,
    pub total: usize,
    pub already_have: Vec<AlreadyHave>,
    pub reason: Option<String>,
}

impl AudiobookSearch {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            result: SearchResult::Unavailable,
            releases: Vec::new(),
            total: 0,
            already_have: Vec::new(),
            reason: Some(reason.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrabOutcome {
    /// Deluge has the torrent; follow it with media_audiobooks_status.
    Grabbed,
    /// The add may have reached Deluge; it is settled by reading, never resent.
    Uncertain,
    /// Deluge refused the torrent.
    Failed,
    ReleaseNotFound,
    /// The same book is already in the library or being downloaded.
    AlreadyHave,
    /// This torrent was grabbed before and is not cancelled or failed.
    AlreadyGrabbed,
    /// Deluge already had the torrent from elsewhere; Omni leaves it alone.
    AlreadyInDeluge,
    /// The Deluge label is missing, does not move finished downloads, or moves
    /// them outside the Plex library.
    NotSetUp,
    /// MyAnonamouse refused a VIP torrent to a non-VIP account; the request
    /// is held and retried (see [`pending`]).
    AwaitingVip,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrabSummary {
    pub outcome: GrabOutcome,
    pub grab_id: Option<String>,
    pub release: Option<AudiobookRelease>,
    pub already_have: Vec<AlreadyHave>,
    pub reason: Option<String>,
}

impl GrabSummary {
    fn of(outcome: GrabOutcome) -> Self {
        Self {
            outcome,
            grab_id: None,
            release: None,
            already_have: Vec::new(),
            reason: None,
        }
    }

    fn because(outcome: GrabOutcome, reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::of(outcome)
        }
    }
}

#[derive(Clone, Debug)]
pub struct GrabRequest {
    pub query: String,
    pub release_id: String,
    pub force: bool,
}

/// Where a grab is on its way into the library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Reserved before the add; blocks a repeat.
    Reserved,
    /// The add's outcome is unknown; settled by reading Deluge.
    Uncertain,
    Downloading,
    /// Moved to the library folder with audio; waiting for Plex.
    Finished,
    /// Plex has the album.
    Ready,
    Cancelled,
    /// Ended without reaching the library (see `problem`).
    Failed,
}

impl Stage {
    /// Not cancelled or failed: blocks another grab of the same torrent.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Cancelled | Self::Failed)
    }

    /// Still on its way: the watcher follows it.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            Self::Reserved | Self::Uncertain | Self::Downloading | Self::Finished
        )
    }
}

/// Why a grab needs attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Problem {
    /// No download progress for a long time.
    Stalled,
    /// Finished, but Plex has not picked it up.
    NotInPlex,
    /// Finished without any audio file.
    NoAudio,
    /// The torrent disappeared from Deluge before finishing.
    Missing,
    /// The add never showed up in Deluge.
    NeverAdded,
    /// Deluge reports an error for the torrent.
    DelugeError,
    /// Plex imported the files without a book title (they carry no tags)
    /// and Omni could not name the album.
    Untagged,
}

/// A notification's delivery, reserved as `sending` before the push.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notice {
    Sending,
    Sent,
    /// The push may have gone out; never resent.
    Uncertain,
    Failed,
}

/// Durable per-torrent grab, keyed by info hash.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookGrabData {
    pub info_hash: String,
    pub release_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<String>,
    pub stage: Stage,
    pub requested_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<f64>,
    /// When progress last advanced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    /// The folder or file the download landed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plex_album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plex_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<Problem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_notice: Option<Notice>,
    /// Problems already notified (at most once each).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerted: Vec<Problem>,
    /// Every audio file of the finished download (full paths); ready needs
    /// Plex to have all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_files: Vec<String>,
    /// When Deluge first stopped listing the torrent (one missing read is
    /// not enough to fail it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_since: Option<i64>,
    /// When Omni last named an untagged import in Plex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagged_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for AudiobookGrabData {
    const NAME: &'static str = "media-audiobook-grab";
    type Key = String;
    fn key(&self) -> String {
        self.info_hash.clone()
    }
}

fn ttl() -> UpsertOpts {
    UpsertOpts {
        ttl_ms: Some(GRAB_RECORD_TTL_MS),
        ..UpsertOpts::default()
    }
}

impl AudiobookGrabData {
    /// The book's name for people: the parsed title and author, else the release title.
    pub fn display(&self) -> String {
        match (&self.book, &self.author) {
            (Some(book), Some(author)) => format!("{book} by {author}"),
            (Some(book), None) => book.clone(),
            _ => self.title.clone(),
        }
    }

    /// Words identifying the book, for duplicate checks.
    fn identity(&self) -> Vec<String> {
        identity(
            self.book.as_deref().unwrap_or(&self.title),
            self.author.as_deref(),
        )
    }
}

fn identity(book: &str, author: Option<&str>) -> Vec<String> {
    let mut w = words(book);
    if let Some(author) = author {
        w.extend(words(author));
    }
    w
}

/// `needle` words all appear in `hay` words.
fn contains_all(hay: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && needle.iter().all(|w| hay.contains(w))
}

/// One grab as status reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrabStatus {
    pub grab_id: String,
    pub title: String,
    pub book: String,
    #[serde(skip)]
    pub safe_title: Option<String>,
    pub stage: Stage,
    pub progress_percent: Option<f64>,
    pub eta_secs: Option<i64>,
    pub seeds: Option<i64>,
    pub download_kbps: Option<i64>,
    /// Deluge's own state (Downloading, Seeding, Paused, Error, ...).
    pub deluge_state: Option<String>,
    pub problem: Option<Problem>,
    pub problem_detail: Option<String>,
    pub plex_album: Option<String>,
    pub plex_artist: Option<String>,
    pub notified: bool,
    pub requested_at: i64,
    pub finished_at: Option<i64>,
    /// Cancel is possible: Omni added it and it has not finished.
    pub cancellable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookStatus {
    pub grabs: Vec<GrabStatus>,
    pub total: usize,
    /// Requests held for MyAnonamouse VIP, newest first.
    pub pending: Vec<pending::PendingStatus>,
    /// Live Deluge progress could not be read; stored progress is shown.
    pub deluge_unreadable: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelOutcome {
    /// Removed from Deluge with its partial data.
    Cancelled,
    /// Already gone from Deluge; recorded as cancelled.
    AlreadyGone,
    /// Finished downloads keep seeding and stay in the library.
    Finished,
    /// Not an open grab Omni made.
    NotFound,
    /// Removal may have happened; read status again.
    Uncertain,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelSummary {
    pub outcome: CancelOutcome,
    pub grab_id: String,
    pub book: Option<String>,
    pub reason: Option<String>,
}

/// One audiobook (a Plex album) in the library.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryAudiobook {
    pub title: String,
    pub author: Option<String>,
    pub year: Option<i64>,
    /// Unix ms.
    pub added_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudiobookLibrary {
    pub library: String,
    /// Newest first.
    pub books: Vec<LibraryAudiobook>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AudiobookError {
    #[error("{0}")]
    Plex(String),
    #[error("the Plex library {0:?} was not found or is not a music library")]
    LibraryNotFound(String),
    #[error("grab records could not be read: {0}")]
    Store(String),
}

/// A side effect recorded instead of performed (`SideEffectMode::Record`).
#[derive(Clone, Debug, PartialEq)]
pub enum Recorded {
    Add {
        release_id: String,
        label: String,
    },
    Remove {
        info_hash: String,
    },
    PlexScan {
        path: String,
    },
    PlexTag {
        album_key: String,
        title: String,
        artist: String,
    },
}

enum Reply<T> {
    Ok(T),
    /// The status and Prowlarr's own `message`, when it sent one.
    Http(u16, Option<String>),
    Unavailable,
}

/// Why the `.torrent` could not be downloaded.
enum TorrentFailure {
    Http(u16),
    Other(String),
}

impl TorrentFailure {
    fn reason(self) -> String {
        match self {
            Self::Http(code) => format!("the .torrent download answered HTTP {code}"),
            Self::Other(reason) => reason,
        }
    }
}

#[derive(Deserialize)]
struct LogPage {
    #[serde(default)]
    records: Vec<pending::LogRecord>,
}

/// Prowlarr's error body (`{"message": ...}`).
#[derive(Deserialize)]
struct ProwlarrError {
    message: Option<String>,
}

/// Prowlarr's error text made safe to return: URLs (which can carry
/// passkeys in their path) are dropped, secrets redacted, whitespace
/// collapsed, and the result bounded.
fn error_message(raw: &str) -> Option<String> {
    static URL: LazyLock<Option<Regex>> =
        LazyLock::new(|| Regex::new(r"(?i)\b[a-z][a-z0-9+.-]*://\S+").ok());
    let without_urls = URL.as_ref().map_or_else(
        || raw.to_owned(),
        |re| re.replace_all(raw, "<url>").into_owned(),
    );
    let redacted = redact(&without_urls);
    let text = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = text.chars().take(200).collect();
    (!bounded.is_empty()).then_some(bounded)
}

/// `HTTP <code>`, plus Prowlarr's explanation when it gave one.
fn http_failure(what: &str, code: u16, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{what} answered HTTP {code}: {message}"),
        None => format!("{what} answered HTTP {code}"),
    }
}

pub struct AudiobooksConfig {
    pub prowlarr_url: Option<String>,
    pub prowlarr_api_key: Option<String>,
    pub indexer: String,
    pub language: String,
    pub library: String,
    pub label: String,
    pub plex_url: Option<String>,
    pub plex_token: Option<String>,
}

#[derive(Clone)]
pub struct Audiobooks {
    http: HttpClient,
    mode: SideEffectMode,
    prowlarr: Option<(String, String)>,
    indexer: String,
    language: String,
    library: String,
    label: String,
    plex: Result<PlexClient, String>,
    plex_scan: Option<(String, String)>,
    deluge: Arc<dyn TorrentClient>,
    store: Store,
    tracker: TaskTracker,
    pushover: Pushover,
    writes: Arc<tokio::sync::Mutex<()>>,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl Audiobooks {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: HttpClient,
        mode: SideEffectMode,
        config: AudiobooksConfig,
        plex: Result<PlexClient, String>,
        deluge: Arc<dyn TorrentClient>,
        store: Store,
        tracker: TaskTracker,
        pushover: Pushover,
    ) -> Self {
        let present = |v: Option<String>| v.filter(|v| !v.is_empty());
        Self {
            http,
            mode,
            prowlarr: present(config.prowlarr_url).zip(present(config.prowlarr_api_key)),
            indexer: config.indexer,
            language: config.language,
            library: config.library,
            label: config.label,
            plex,
            plex_scan: present(config.plex_url).zip(present(config.plex_token)),
            deluge,
            store,
            tracker,
            pushover,
            writes: Arc::new(tokio::sync::Mutex::new(())),
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Prowlarr, Deluge and Plex are all configured.
    pub fn is_configured(&self) -> bool {
        self.prowlarr.is_some() && self.deluge.is_configured() && self.plex.is_ok()
    }

    /// Side effects recorded in `SideEffectMode::Record`.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, side_effect: Recorded) {
        tracing::info!(target: LOG, "Recorded {side_effect:?} (side effects recorded, not sent)");
        if let Ok(mut recorded) = self.recorded.lock() {
            recorded.push(side_effect);
        }
    }

    /// Ranked releases on the audiobook indexer. Grabs nothing.
    pub async fn search(&self, query: &str) -> AudiobookSearch {
        let ranked = match self.ranked(query).await {
            Ok(ranked) => ranked,
            Err(reason) => return AudiobookSearch::unavailable(reason),
        };
        let total = ranked.len();
        let releases = ranked
            .into_iter()
            .take(RELEASE_LIMIT)
            .map(|(release, _)| release)
            .collect();
        AudiobookSearch {
            result: SearchResult::Listed,
            releases,
            total,
            already_have: self.already_have(&words(query)).await,
            reason: None,
        }
    }

    async fn ranked(
        &self,
        query: &str,
    ) -> Result<Vec<(AudiobookRelease, ProwlarrRelease)>, String> {
        let connection = self.connection().ok_or("Prowlarr is not configured")?;
        let indexer_id = self.indexer_id(connection).await?;
        let raw = self.find(connection, indexer_id, query).await?;
        let mut assessed: Vec<(Assessment, ProwlarrRelease)> = raw
            .into_iter()
            .filter(|r| r.protocol.as_deref() == Some("torrent"))
            .filter_map(|r| {
                let candidate = Candidate {
                    title: r.title.clone()?,
                    seeders: r.seeders,
                    freeleech: r.freeleech(),
                };
                rank::assess(query, &self.language, &candidate).map(|a| (a, r))
            })
            .collect();
        rank::sort(&mut assessed);
        Ok(assessed
            .into_iter()
            .enumerate()
            .filter_map(|(i, (a, r))| AudiobookRelease::of(i + 1, &r, a).map(|out| (out, r)))
            .collect())
    }

    /// Library albums and open grabs whose words include all of `wanted`.
    async fn already_have(&self, wanted: &[String]) -> Vec<AlreadyHave> {
        let mut have = Vec::new();
        if let Ok(library) = self.library(None).await {
            have.extend(
                library
                    .books
                    .into_iter()
                    .filter(|b| contains_all(&identity(&b.title, b.author.as_deref()), wanted))
                    .map(|b| AlreadyHave {
                        safe_title: Some(b.title.clone()),
                        title: b.title,
                        author: b.author,
                        status: "in_library".to_owned(),
                        grab_id: None,
                    }),
            );
        }
        if let Ok(grabs) = self.grabs().await {
            have.extend(
                grabs
                    .into_iter()
                    .filter(|g| g.stage.is_open() && contains_all(&g.identity(), wanted))
                    .map(|g| AlreadyHave {
                        safe_title: g.book.clone(),
                        title: g.book.clone().unwrap_or_else(|| g.title.clone()),
                        author: g.author.clone(),
                        status: serde_json::to_value(g.stage)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        grab_id: Some(g.info_hash),
                    }),
            );
        }
        have
    }

    /// Runs to completion once started (a dropped request must not split the
    /// reservation from the add); only the reservation and add hold the write
    /// lock, not the search or the torrent download.
    pub async fn grab(&self, request: GrabRequest) -> GrabSummary {
        let this = self.clone();
        must_complete(&self.tracker, async move { this.grab_inner(request).await }).await
    }

    async fn grab_inner(&self, request: GrabRequest) -> GrabSummary {
        let Some(connection) = self.connection() else {
            return GrabSummary::because(GrabOutcome::Unavailable, "Prowlarr is not configured");
        };
        if !self.deluge.is_configured() {
            return GrabSummary::because(GrabOutcome::Unavailable, "Deluge is not configured");
        }
        let completed = match self.check_setup().await {
            Ok(path) => path,
            Err(summary) => return *summary,
        };
        // A fresh search proves the release is still offered.
        let ranked = match self.ranked(&request.query).await {
            Ok(ranked) => ranked,
            Err(reason) => return GrabSummary::because(GrabOutcome::Unavailable, reason),
        };
        let Some((release, found)) = ranked
            .into_iter()
            .find(|(r, _)| r.release_id == request.release_id)
        else {
            return GrabSummary::of(GrabOutcome::ReleaseNotFound);
        };
        if !request.force {
            let have = self
                .already_have(&identity(
                    release.book.as_deref().unwrap_or(&release.title),
                    None,
                ))
                .await;
            if !have.is_empty() {
                self.settle_pending(&release.release_id, None).await;
                return GrabSummary {
                    already_have: have,
                    release: Some(release),
                    ..GrabSummary::of(GrabOutcome::AlreadyHave)
                };
            }
        }
        // Downloading the .torrent counts as a snatch on a private tracker,
        // so every check that can refuse runs first.
        match self.grabs().await {
            Ok(grabs)
                if grabs
                    .iter()
                    .any(|g| g.release_id == release.release_id && g.stage.is_active()) =>
            {
                self.settle_pending(&release.release_id, None).await;
                return GrabSummary {
                    release: Some(release),
                    ..GrabSummary::of(GrabOutcome::AlreadyGrabbed)
                };
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(target: LOG, "Audiobook grab records unreadable: {error}");
                return GrabSummary::of(GrabOutcome::Error);
            }
        }
        if self.mode == SideEffectMode::Record {
            self.record(Recorded::Add {
                release_id: release.release_id.clone(),
                label: self.label.clone(),
            });
            return GrabSummary {
                release: Some(release),
                ..GrabSummary::of(GrabOutcome::Grabbed)
            };
        }
        let started = self.store.clock().now_ms();
        let torrent_bytes = match self.download_torrent(connection, &found).await {
            Ok(bytes) => bytes,
            Err(failure) => {
                // Prowlarr reports MyAnonamouse's 406 "not VIP" refusal as a
                // bare 500; only its log tells that refusal from other errors.
                let vip = found.title.as_deref().is_some_and(rank::is_vip);
                if vip
                    && matches!(failure, TorrentFailure::Http(code) if code >= 500)
                    && self.vip_refused(connection, started).await
                {
                    self.hold_for_vip(&request, &release).await;
                    return GrabSummary {
                        release: Some(release),
                        ..GrabSummary::because(GrabOutcome::AwaitingVip, pending::VIP_ACTION)
                    };
                }
                let reason = failure.reason();
                return GrabSummary {
                    release: Some(release),
                    ..GrabSummary::because(GrabOutcome::Unavailable, reason)
                };
            }
        };
        let parsed = match torrent::parse(&torrent_bytes) {
            Ok(parsed) => parsed,
            Err(_) => {
                return GrabSummary {
                    release: Some(release),
                    ..GrabSummary::because(
                        GrabOutcome::Unavailable,
                        "Prowlarr did not return a .torrent file",
                    )
                };
            }
        };
        let hash = parsed.info_hash.clone();
        let _writes = self.writes.lock().await;
        let row = AudiobookGrabData {
            info_hash: hash.clone(),
            release_id: release.release_id.clone(),
            title: release.title.clone(),
            book: release.book.clone(),
            author: release.author.clone(),
            indexer: found.indexer.clone(),
            stage: Stage::Reserved,
            requested_at: 0,
            updated_at: 0,
            progress_percent: None,
            progress_at: None,
            finished_at: None,
            path: None,
            scanned_at: None,
            plex_album: None,
            plex_artist: None,
            problem: None,
            problem_detail: None,
            ready_notice: None,
            alerted: Vec::new(),
            audio_files: Vec::new(),
            missing_since: None,
            tagged_at: None,
            extra: Extra::default(),
        };
        let summary = |outcome| GrabSummary {
            grab_id: Some(hash.clone()),
            release: Some(release.clone()),
            ..GrabSummary::of(outcome)
        };
        let previous = match self.reserve(row).await {
            Ok(Ok(previous)) => previous,
            Ok(Err(())) => return summary(GrabOutcome::AlreadyGrabbed),
            Err(error) => {
                tracing::warn!(target: LOG, "Audiobook grab record failed: {error}");
                return summary(GrabOutcome::Error);
            }
        };
        let file_name = format!(
            "{}.torrent",
            parsed.name.as_deref().unwrap_or(hash.as_str())
        );
        let added = self
            .deluge
            .add(&file_name, &torrent_bytes, &hash, &self.label, &completed)
            .await;
        let outcome = match added {
            Ok(Added::Added) => self.verify_added(&hash).await,
            Ok(Added::AlreadyPresent) => {
                tracing::info!(target: LOG, "Deluge already had {}; not taking it over", release.title);
                self.release_reservation(&hash, previous).await;
                return GrabSummary {
                    reason: Some(
                        "Deluge already has this torrent; Omni did not add it and will not manage it"
                            .to_owned(),
                    ),
                    ..summary(GrabOutcome::AlreadyInDeluge)
                };
            }
            Err(deluge::AddError::NotSent(error)) => {
                tracing::warn!(target: LOG, "Audiobook add not sent: {error}");
                self.release_reservation(&hash, previous).await;
                return GrabSummary {
                    reason: Some(error.to_string()),
                    ..summary(GrabOutcome::Unavailable)
                };
            }
            Err(deluge::AddError::Refused(error)) => {
                tracing::warn!(target: LOG, "Deluge refused {}: {error}", release.title);
                self.release_reservation(&hash, previous).await;
                return GrabSummary {
                    reason: Some(error.to_string()),
                    ..summary(GrabOutcome::Failed)
                };
            }
            Err(deluge::AddError::Unknown(error)) => {
                tracing::warn!(target: LOG, "Audiobook add outcome unknown: {error}");
                self.verify_added(&hash).await
            }
        };
        let stage = if outcome == GrabOutcome::Grabbed {
            Stage::Downloading
        } else {
            Stage::Uncertain
        };
        self.update(&hash, move |row, now| {
            row.stage = stage;
            row.updated_at = now;
            row.progress_at = Some(now);
        })
        .await;
        if outcome == GrabOutcome::Grabbed {
            tracing::info!(target: LOG, "Added audiobook {} to Deluge", release.title);
        }
        self.settle_pending(&release.release_id, Some(hash.clone()))
            .await;
        summary(outcome)
    }

    /// The label's completed folder, verified to be inside the Plex library.
    async fn check_setup(&self) -> Result<String, Box<GrabSummary>> {
        let not_set_up =
            |reason: String| Box::new(GrabSummary::because(GrabOutcome::NotSetUp, reason));
        let completed = match self.deluge.label_completed_path(&self.label).await {
            Ok(Some(path)) => path,
            Ok(None) => {
                return Err(not_set_up(format!(
                    "Deluge label {:?} is missing or does not move finished downloads",
                    self.label
                )));
            }
            Err(error) => {
                return Err(Box::new(GrabSummary::because(
                    GrabOutcome::Unavailable,
                    error.to_string(),
                )));
            }
        };
        let section = self
            .section()
            .await
            .map_err(|reason| Box::new(GrabSummary::because(GrabOutcome::Unavailable, reason)))?;
        if !section.contains(&completed) {
            return Err(not_set_up(format!(
                "Deluge label {:?} moves downloads to {completed}, which is not a location of the Plex library {:?}",
                self.label, self.library
            )));
        }
        Ok(completed)
    }

    async fn section(&self) -> Result<MusicSection, String> {
        let plex = self.plex.as_ref().map_err(Clone::clone)?;
        plex.music_section(&self.library)
            .await
            .map_err(|e| redact(e.cause_message()))?
            .ok_or_else(|| AudiobookError::LibraryNotFound(self.library.clone()).to_string())
    }

    /// Grabbed when Deluge lists the torrent; otherwise uncertain.
    async fn verify_added(&self, hash: &str) -> GrabOutcome {
        match self.deluge.statuses(&[hash.to_owned()]).await {
            Ok(statuses) if statuses.contains_key(hash) => GrabOutcome::Grabbed,
            Ok(_) => GrabOutcome::Uncertain,
            Err(error) => {
                tracing::warn!(target: LOG, "Audiobook add could not be verified: {error}");
                GrabOutcome::Uncertain
            }
        }
    }

    /// Grabs, newest first, optionally only those whose words include all of
    /// `query`'s; live Deluge progress for open ones.
    pub async fn status(&self, query: Option<&str>) -> Result<AudiobookStatus, AudiobookError> {
        let wanted = query.map(words).filter(|w| !w.is_empty());
        let mut grabs: Vec<AudiobookGrabData> = self
            .grabs()
            .await
            .map_err(|e| AudiobookError::Store(e.to_string()))?
            .into_iter()
            .filter(|g| {
                wanted.as_ref().is_none_or(|w| {
                    contains_all(&g.identity(), w)
                        || (w.len() == 1
                            && w[0].len() >= 8
                            && w[0].bytes().all(|b| b.is_ascii_hexdigit())
                            && g.info_hash.starts_with(&w[0]))
                })
            })
            .collect();
        grabs.sort_by_key(|g| std::cmp::Reverse(g.requested_at));
        let total = grabs.len();
        grabs.truncate(STATUS_LIMIT);
        let open: Vec<String> = grabs
            .iter()
            .filter(|g| g.stage.is_open())
            .map(|g| g.info_hash.clone())
            .collect();
        let (live, deluge_unreadable) = match self.deluge.statuses(&open).await {
            Ok(live) => (live, None),
            Err(error) => (Default::default(), Some(error.to_string())),
        };
        let pending = self
            .pending_status(wanted.as_deref())
            .await
            .map_err(|e| AudiobookError::Store(e.to_string()))?;
        Ok(AudiobookStatus {
            pending,
            grabs: grabs
                .into_iter()
                .map(|g| {
                    let now = live.get(&g.info_hash);
                    GrabStatus {
                        safe_title: g.book.clone(),
                        cancellable: matches!(
                            g.stage,
                            Stage::Reserved | Stage::Uncertain | Stage::Downloading
                        ) && !now.is_some_and(|s| s.is_finished),
                        grab_id: g.info_hash.clone(),
                        book: g.display(),
                        title: g.title.clone(),
                        stage: g.stage,
                        progress_percent: now.map(|s| s.progress_percent).or(g.progress_percent),
                        eta_secs: now.and_then(|s| s.eta_secs),
                        seeds: now.map(|s| s.seeds),
                        download_kbps: now.map(|s| s.download_rate / 1000),
                        deluge_state: now.map(|s| s.state.clone()),
                        problem: g.problem,
                        problem_detail: g.problem_detail.clone(),
                        plex_album: g.plex_album.clone(),
                        plex_artist: g.plex_artist.clone(),
                        notified: g.ready_notice == Some(Notice::Sent),
                        requested_at: g.requested_at,
                        finished_at: g.finished_at,
                    }
                })
                .collect(),
            total,
            deluge_unreadable,
        })
    }

    pub(crate) async fn resolve_addition_id(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.store
            .read({
                let id = id.to_owned();
                move |tx| {
                    let pending = tx.get_all::<pending::AudiobookPendingData>()?;
                    let grabs = tx.get_all::<AudiobookGrabData>()?;
                    Ok(pending
                        .into_iter()
                        .map(|row| row.release_id)
                        .chain(grabs.into_iter().map(|row| row.info_hash))
                        .find(|raw| addition_id(raw) == id))
                }
            })
            .await
    }

    /// Removes an unfinished grab and its partial data from Deluge.
    pub async fn cancel(&self, grab_id: &str) -> CancelSummary {
        let this = self.clone();
        let grab_id = grab_id.to_ascii_lowercase();
        must_complete(
            &self.tracker,
            async move { this.cancel_inner(grab_id).await },
        )
        .await
    }

    async fn cancel_inner(&self, hash: String) -> CancelSummary {
        let summary = |outcome, book: Option<String>, reason: Option<String>| CancelSummary {
            outcome,
            grab_id: hash.clone(),
            book,
            reason,
        };
        // A release id names a request held for VIP; nothing is in Deluge.
        if hash.len() == 32 {
            return match self.cancel_pending(&hash).await {
                Some(book) => summary(CancelOutcome::Cancelled, Some(book), None),
                None => summary(CancelOutcome::NotFound, None, None),
            };
        }
        let _writes = self.writes.lock().await;
        let row = match self
            .store
            .read({
                let hash = hash.clone();
                move |tx| tx.get::<AudiobookGrabData>(&hash)
            })
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => return summary(CancelOutcome::NotFound, None, None),
            Err(error) => return summary(CancelOutcome::Error, None, Some(error.to_string())),
        };
        let book = Some(row.display());
        match row.stage {
            Stage::Finished | Stage::Ready => {
                return summary(
                    CancelOutcome::Finished,
                    book,
                    Some("finished downloads keep seeding (MyAnonamouse requires it)".to_owned()),
                );
            }
            Stage::Cancelled | Stage::Failed => {
                return summary(CancelOutcome::NotFound, book, None);
            }
            Stage::Reserved | Stage::Uncertain | Stage::Downloading => {}
        }
        let mut present = None;
        // Two reads: a daemon that is just starting can list nothing.
        for _ in 0..2 {
            present = match self.deluge.statuses(std::slice::from_ref(&hash)).await {
                Ok(statuses) => statuses.get(&hash).cloned(),
                Err(error) => {
                    return summary(CancelOutcome::Unavailable, book, Some(error.to_string()));
                }
            };
            if present.is_some() {
                break;
            }
        }
        let Some(status) = present else {
            self.update(&hash, |row, now| {
                row.stage = Stage::Cancelled;
                row.updated_at = now;
            })
            .await;
            return summary(CancelOutcome::AlreadyGone, book, None);
        };
        if status.is_finished {
            return summary(
                CancelOutcome::Finished,
                book,
                Some("the download just finished; it keeps seeding".to_owned()),
            );
        }
        if self.mode == SideEffectMode::Record {
            self.record(Recorded::Remove {
                info_hash: hash.clone(),
            });
            return summary(CancelOutcome::Cancelled, book, None);
        }
        let removed = self.deluge.remove(&hash).await;
        let gone = match self.deluge.statuses(std::slice::from_ref(&hash)).await {
            Ok(statuses) => Some(!statuses.contains_key(&hash)),
            Err(_) => None,
        };
        match (removed, gone) {
            (_, Some(true)) => {
                self.update(&hash, |row, now| {
                    row.stage = Stage::Cancelled;
                    row.updated_at = now;
                })
                .await;
                tracing::info!(target: LOG, "Cancelled audiobook {}", row.title);
                summary(CancelOutcome::Cancelled, book, None)
            }
            (Err(error), _) => summary(CancelOutcome::Uncertain, book, Some(error.to_string())),
            (Ok(()), _) => summary(
                CancelOutcome::Uncertain,
                book,
                Some("Deluge still lists the torrent".to_owned()),
            ),
        }
    }

    /// The Plex audiobook library, optionally filtered by title or author.
    pub async fn library(&self, query: Option<&str>) -> Result<AudiobookLibrary, AudiobookError> {
        let plex = self
            .plex
            .as_ref()
            .map_err(|r| AudiobookError::Plex(r.clone()))?;
        let albums = plex
            .fetch_music_albums(&self.library)
            .await
            .map_err(|e| AudiobookError::Plex(redact(e.cause_message())))?
            .ok_or_else(|| AudiobookError::LibraryNotFound(self.library.clone()))?;
        let wanted = query.map(words).filter(|w| !w.is_empty());
        #[allow(clippy::cast_possible_truncation)]
        let mut books: Vec<LibraryAudiobook> = albums
            .into_iter()
            .filter_map(|album| {
                let title = album.title?;
                let author = album.parent_title;
                wanted
                    .as_ref()
                    .is_none_or(|w| contains_all(&identity(&title, author.as_deref()), w))
                    .then(|| LibraryAudiobook {
                        title,
                        author,
                        year: album.year.map(|y| y as i64),
                        added_at: album.added_at.map(|s| (s as i64).saturating_mul(1000)),
                    })
            })
            .collect();
        books.sort_by_key(|b| std::cmp::Reverse(b.added_at));
        Ok(AudiobookLibrary {
            library: self.library.clone(),
            books,
        })
    }

    async fn grabs(&self) -> Result<Vec<AudiobookGrabData>, StoreError> {
        self.store
            .read(|tx| tx.get_all::<AudiobookGrabData>())
            .await
    }

    /// `Ok(Err(()))` when an active grab of the torrent blocks a new one;
    /// `force` never overrides that, so an uncertain add is never resent.
    async fn reserve(
        &self,
        row: AudiobookGrabData,
    ) -> Result<Result<Option<AudiobookGrabData>, ()>, StoreError> {
        self.store
            .write(move |tx| {
                let now = tx.now_ms();
                let previous = tx.get::<AudiobookGrabData>(&row.info_hash)?;
                let active = previous.as_ref().is_some_and(|p| p.stage.is_active());
                if active {
                    return Ok::<_, StoreError>(Err(()));
                }
                tx.upsert(
                    &AudiobookGrabData {
                        requested_at: now,
                        updated_at: now,
                        ..row
                    },
                    ttl(),
                )?;
                Ok(Ok(previous))
            })
            .await
    }

    /// Restores what a definitely failed add's reservation replaced.
    async fn release_reservation(&self, hash: &str, previous: Option<AudiobookGrabData>) {
        let key = hash.to_owned();
        let released = self
            .store
            .write(move |tx| match previous {
                Some(previous) => tx.upsert(&previous, ttl()),
                None => tx.delete::<AudiobookGrabData>(&key).map(|_| ()),
            })
            .await;
        if let Err(error) = released {
            // The reservation stays and blocks a repeat until cancelled.
            tracing::warn!(target: LOG, "Audiobook grab record failed: {error}");
        }
    }

    /// Applies `change(row, now)` to a stored grab.
    async fn update(
        &self,
        hash: &str,
        change: impl FnOnce(&mut AudiobookGrabData, i64) + Send + 'static,
    ) {
        let key = hash.to_owned();
        let updated = self
            .store
            .write(move |tx| {
                let now = tx.now_ms();
                let Some(mut row) = tx.get::<AudiobookGrabData>(&key)? else {
                    return Ok::<_, StoreError>(());
                };
                change(&mut row, now);
                tx.upsert(&row, ttl())
            })
            .await;
        if let Err(error) = updated {
            tracing::warn!(target: LOG, "Audiobook grab record failed: {error}");
        }
    }

    fn connection(&self) -> Option<(&str, &str)> {
        self.prowlarr
            .as_ref()
            .map(|(url, key)| (url.as_str(), key.as_str()))
    }

    async fn indexer_id(&self, connection: (&str, &str)) -> Result<i64, String> {
        match self
            .send::<Vec<IndexerResource>>(connection, "indexer", &[], READ_TIMEOUT, READ_MAX_BYTES)
            .await
        {
            Reply::Ok(indexers) => indexers
                .iter()
                .find(|i| {
                    i.enable
                        && i.name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(&self.indexer))
                })
                .and_then(|i| i.id)
                .ok_or_else(|| format!("Prowlarr has no enabled indexer named {}", self.indexer)),
            Reply::Http(code, message) => {
                Err(http_failure("Prowlarr indexers", code, message.as_deref()))
            }
            Reply::Unavailable => Err("Prowlarr could not be reached".to_owned()),
        }
    }

    async fn find(
        &self,
        connection: (&str, &str),
        indexer_id: i64,
        query: &str,
    ) -> Result<Vec<ProwlarrRelease>, String> {
        let indexer = indexer_id.to_string();
        let params = [
            ("query", query),
            ("categories", AUDIOBOOK_CATEGORY),
            ("indexerIds", indexer.as_str()),
            ("type", "search"),
            ("limit", "100"),
        ];
        match self
            .send::<Vec<Value>>(
                connection,
                "search",
                &params,
                SEARCH_TIMEOUT,
                SEARCH_MAX_BYTES,
            )
            .await
        {
            Reply::Ok(raw) => Ok(lenient_releases(raw)),
            Reply::Http(code, message) => {
                Err(http_failure("Prowlarr search", code, message.as_deref()))
            }
            Reply::Unavailable => Err("Prowlarr search failed or timed out".to_owned()),
        }
    }

    /// Whether Prowlarr logged MyAnonamouse's VIP refusal since `started`.
    /// Prowlarr writes its log asynchronously, so a miss is read again.
    async fn vip_refused(&self, connection: (&str, &str), started: i64) -> bool {
        let params = [
            ("page", "1"),
            ("pageSize", "50"),
            ("sortKey", "time"),
            ("sortDirection", "descending"),
        ];
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            match self
                .send::<LogPage>(connection, "log", &params, READ_TIMEOUT, READ_MAX_BYTES)
                .await
            {
                Reply::Ok(page)
                    if pending::vip_refusal(
                        &page.records,
                        started,
                        self.store.clock().now_ms(),
                    ) =>
                {
                    return true;
                }
                Reply::Ok(_) => {}
                Reply::Http(code, _) => {
                    tracing::debug!(target: LOG, "Prowlarr log answered HTTP {code}");
                    return false;
                }
                Reply::Unavailable => return false,
            }
        }
        false
    }

    /// The `.torrent` through Prowlarr's download proxy. Only the path and
    /// query of Prowlarr's `downloadUrl` are used, against the configured
    /// Prowlarr origin, so the request never goes anywhere else.
    async fn download_torrent(
        &self,
        (base, api_key): (&str, &str),
        release: &ProwlarrRelease,
    ) -> Result<Vec<u8>, TorrentFailure> {
        let source = release
            .download_url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| TorrentFailure::Other("the release has no download link".to_owned()))?;
        let mut url = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .map_err(|_| TorrentFailure::Other("invalid PROWLARR_URL".to_owned()))?;
        url.set_path(source.path());
        url.set_query(source.query());
        let request = self
            .http
            .request(Method::GET, url)
            .header("X-Api-Key", api_key);
        let response = tokio::time::timeout(READ_TIMEOUT, request.send_bounded(TORRENT_MAX_BYTES))
            .await
            .map_err(|_| TorrentFailure::Other("the .torrent download timed out".to_owned()))?
            .map_err(|e| {
                tracing::debug!(target: LOG, "Torrent download failed: {}", redact(&e.to_string()));
                TorrentFailure::Other("the .torrent download failed".to_owned())
            })?;
        if !response.status.is_success() {
            return Err(TorrentFailure::Http(response.status.as_u16()));
        }
        Ok(response.body.to_vec())
    }

    async fn send<T: DeserializeOwned>(
        &self,
        (base, api_key): (&str, &str),
        path: &str,
        params: &[(&str, &str)],
        timeout: Duration,
        max_bytes: usize,
    ) -> Reply<T> {
        let Some(mut url) = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .ok()
            .and_then(|b| b.join(&format!("api/v1/{path}")).ok())
        else {
            return Reply::Unavailable;
        };
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        let request = self
            .http
            .request(Method::GET, url)
            .header("Accept", "application/json")
            .header("X-Api-Key", api_key);
        let response = match tokio::time::timeout(timeout, request.send_bounded(max_bytes)).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::debug!(target: LOG, "Prowlarr {path} failed: {}", redact(&error.to_string()));
                return Reply::Unavailable;
            }
            Err(_) => {
                tracing::debug!(target: LOG, "Prowlarr {path} timed out");
                return Reply::Unavailable;
            }
        };
        if !response.status.is_success() {
            let message = serde_json::from_slice::<ProwlarrError>(&response.body)
                .ok()
                .and_then(|e| e.message)
                .and_then(|m| error_message(&m));
            return Reply::Http(response.status.as_u16(), message);
        }
        match serde_json::from_slice::<T>(&response.body) {
            Ok(value) => Reply::Ok(value),
            Err(error) => {
                tracing::debug!(target: LOG, "Prowlarr {path} response rejected: {error}");
                Reply::Unavailable
            }
        }
    }

    /// Asks Plex to scan `path` in the audiobook library.
    async fn plex_scan(&self, section: &str, path: &str) -> Result<(), String> {
        if self.mode == SideEffectMode::Record {
            self.record(Recorded::PlexScan {
                path: path.to_owned(),
            });
            return Ok(());
        }
        let (base, token) = self.plex_scan.as_ref().ok_or("Plex is not configured")?;
        let mut url = Url::parse(&format!("{}/", base.trim_end_matches('/')))
            .and_then(|b| b.join(&format!("library/sections/{section}/refresh")))
            .map_err(|_| "invalid PLEX_URL")?;
        url.query_pairs_mut().append_pair("path", path);
        let request = self
            .http
            .request(Method::GET, url)
            .header("X-Plex-Token", token.as_str());
        let response = tokio::time::timeout(READ_TIMEOUT, request.send_bounded(READ_MAX_BYTES))
            .await
            .map_err(|_| "the Plex scan request timed out".to_owned())?
            .map_err(|_| "the Plex scan request failed".to_owned())?;
        if response.status.is_success() {
            Ok(())
        } else {
            Err(format!(
                "Plex scan answered HTTP {}",
                response.status.as_u16()
            ))
        }
    }
}

/// The folder a finished torrent occupies: its own folder, or the save
/// folder for a single-file torrent. Plex scans folders, not files.
fn landed_path(status: &TorrentStatus) -> String {
    let base = status.save_path.trim_end_matches('/');
    match status.files.first().and_then(|f| f.split_once('/')) {
        Some((top, _)) => format!("{base}/{top}"),
        None => base.to_owned(),
    }
}

/// The folder to scan for `path`: a file's own folder, else `path`.
fn scan_folder(path: &str) -> &str {
    if is_audio(path) {
        path.rsplit_once('/').map_or(path, |(dir, _)| dir)
    } else {
        path
    }
}

fn is_audio(file: &str) -> bool {
    file.rsplit_once('.')
        .is_some_and(|(_, ext)| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn has_audio(status: &TorrentStatus) -> bool {
    status.files.iter().any(|f| is_audio(f))
}

#[cfg(test)]
mod tests {
    use super::deluge::TorrentStatus;
    use super::{error_message, has_audio, landed_path, scan_folder};

    #[test]
    fn prowlarr_error_text_never_carries_urls_or_secrets() {
        let message = error_message(
            "Failed\n  for https://www.myanonamouse.net/tor/download.php?tid=1&passkey=SECRET \
             and /3/download?apikey=KEY",
        )
        .expect("message");
        assert_eq!(
            message,
            "Failed for <url> and /3/download?apikey=<redacted>"
        );
        assert!(error_message("   ").is_none());
        let long = error_message(&"x".repeat(500)).expect("message");
        assert_eq!(long.chars().count(), 200);
    }

    #[test]
    fn finds_where_a_download_landed_and_whether_it_has_audio() {
        let status = TorrentStatus {
            name: "Project Hail Mary".to_owned(),
            save_path: "/media/storage/deluge/books/".to_owned(),
            files: vec![
                "Project Hail Mary/Project Hail Mary.m4b".to_owned(),
                "Project Hail Mary/cover.jpg".to_owned(),
            ],
            ..TorrentStatus::default()
        };
        assert_eq!(
            landed_path(&status),
            "/media/storage/deluge/books/Project Hail Mary"
        );
        let single = TorrentStatus {
            files: vec!["The Martian (Unabridged).m4b".to_owned()],
            ..status.clone()
        };
        assert_eq!(landed_path(&single), "/media/storage/deluge/books");
        assert_eq!(
            scan_folder("/media/storage/deluge/books/The Martian (Unabridged).m4b"),
            "/media/storage/deluge/books"
        );
        assert_eq!(
            scan_folder("/media/storage/deluge/books"),
            "/media/storage/deluge/books"
        );
        assert!(has_audio(&status));
        let ebook = TorrentStatus {
            files: vec!["Book/book.epub".to_owned()],
            ..status
        };
        assert!(!has_audio(&ebook));
    }
}
