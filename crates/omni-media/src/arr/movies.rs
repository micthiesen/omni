//! Movie-level Radarr management: inspect movies and their files, change
//! quality profiles, search for missing movies or upgrades, pick a release
//! by hand, and verify what an upgrade left in the library.
//!
//! Changing a profile never searches. A search or grab is reserved durably
//! per movie before the request, like the Sonarr season searches
//! (`seasons.rs`), so a later call within [`SEARCH_WINDOW_MS`] reports
//! `recently_searched` or `recently_grabbed` instead of acquiring twice; a
//! download in the queue, a Radarr search already running, a grab in Radarr's
//! own history and a post-add search not yet run block it too.
//!
//! Radarr replaces a movie's file itself when it imports an upgrade, so Omni
//! never deletes anything: the original stays until Radarr has imported the
//! replacement. An upgrade is verified only when the movie's file changed,
//! Radarr manages exactly one file, and no other video file sits in the
//! movie's folder.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::spawn::must_complete;
use omni_http::{Method, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{DocOps as _, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::task::TaskTracker;

use super::inspect::{
    ArrService, Download, InspectError, NamedResource, QualityModel, QualityProfileResource,
    QueueRecord, fetch_profiles, fetch_queue, names, read, redact,
};
use super::radarr::{MovieAddLedger, add_radarr_movie_with};
use super::seasons::SearchState;
use super::{ArrConfig, ArrHttp, HttpResult, Limits, WriteOutcome};
use crate::types::AddToWatchlistResult;

const LOG: &str = "Radarr";
const HOUR_MS: i64 = 60 * 60 * 1000;
/// A movie searched or grabbed within this window is not searched or
/// grabbed again unless the caller forces it.
pub const SEARCH_WINDOW_MS: i64 = 24 * HOUR_MS;
const SEARCH_RECORD_TTL_MS: i64 = 7 * 24 * HOUR_MS;
/// Detail reads (files, folder scan, history) run this many movies at once.
const DETAIL_CONCURRENCY: usize = 4;
/// The most releases one listing returns.
pub const RELEASE_LIMIT: usize = 50;
/// Sending a grab to the download client can take a while.
const GRAB_LIMITS: Limits = Limits {
    timeout: Duration::from_secs(60),
    max_bytes: super::ARR_JSON_MAX_BYTES,
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionRef {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tmdb_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieStatistics {
    #[serde(default)]
    movie_file_count: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieAddOptions {
    #[serde(default)]
    search_for_movie: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaInfo {
    #[serde(default)]
    audio_channels: Option<f64>,
    #[serde(default)]
    audio_codec: Option<String>,
    #[serde(default)]
    audio_languages: Option<String>,
    #[serde(default)]
    video_bit_depth: Option<i64>,
    #[serde(default)]
    video_codec: Option<String>,
    #[serde(default)]
    video_dynamic_range: Option<String>,
    #[serde(default)]
    video_dynamic_range_type: Option<String>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    run_time: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieFileResource {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    relative_path: Option<String>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    date_added: Option<String>,
    #[serde(default)]
    scene_name: Option<String>,
    #[serde(default)]
    release_group: Option<String>,
    #[serde(default)]
    edition: Option<String>,
    #[serde(default)]
    languages: Vec<NamedResource>,
    #[serde(default)]
    quality: Option<QualityModel>,
    /// Only `moviefile` reads carry the formats; the movie list does not.
    #[serde(default)]
    custom_formats: Option<Vec<NamedResource>>,
    #[serde(default)]
    custom_format_score: Option<i64>,
    #[serde(default)]
    media_info: Option<MediaInfo>,
    #[serde(default)]
    quality_cutoff_not_met: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieResource {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    tmdb_id: Option<i64>,
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    monitored: Option<bool>,
    #[serde(default)]
    quality_profile_id: Option<i64>,
    #[serde(default)]
    has_file: Option<bool>,
    #[serde(default)]
    is_available: Option<bool>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    collection: Option<CollectionRef>,
    #[serde(default)]
    movie_file: Option<MovieFileResource>,
    #[serde(default)]
    statistics: Option<MovieStatistics>,
    /// Present until Radarr's first scan after the add has run.
    #[serde(default)]
    add_options: Option<MovieAddOptions>,
}

impl MovieResource {
    fn initial_search_pending(&self) -> bool {
        self.add_options
            .as_ref()
            .and_then(|o| o.search_for_movie)
            .unwrap_or(false)
    }

    fn file_id(&self) -> Option<i64> {
        if self.has_file == Some(true) {
            self.movie_file.as_ref().and_then(|f| f.id)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandBody {
    #[serde(default)]
    movie_ids: Option<Vec<i64>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandResource {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    body: Option<CommandBody>,
}

impl CommandResource {
    /// A queued or started search that may grab for `movie_id`: one naming
    /// the movie, or a library-wide missing or cutoff-unmet search.
    fn searches(&self, movie_id: i64) -> bool {
        if !matches!(self.status.as_deref(), Some("queued" | "started")) {
            return false;
        }
        match self.name.as_deref() {
            Some("MissingMoviesSearch" | "CutoffUnmetMoviesSearch") => true,
            Some("MoviesSearch") => self
                .body
                .as_ref()
                .and_then(|b| b.movie_ids.as_ref())
                .is_some_and(|ids| ids.contains(&movie_id)),
            _ => false,
        }
    }
}

/// A Radarr history row. Its `data` holds indexer download URLs with API
/// keys and is never read.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryResource {
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    source_title: Option<String>,
    #[serde(default)]
    quality: Option<QualityModel>,
}

impl HistoryResource {
    fn at_ms(&self) -> Option<i64> {
        self.date
            .as_deref()?
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Rejection {
    #[serde(default)]
    reason: Option<String>,
}

/// Radarr's manual-import view of a file it does not manage.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManualImportResource {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    relative_path: Option<String>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    quality: Option<QualityModel>,
    #[serde(default)]
    rejections: Vec<Rejection>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseResource {
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
    age: Option<i64>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    quality: Option<QualityModel>,
    #[serde(default)]
    custom_formats: Vec<NamedResource>,
    #[serde(default)]
    custom_format_score: Option<i64>,
    #[serde(default)]
    languages: Vec<NamedResource>,
    #[serde(default)]
    release_group: Option<String>,
    #[serde(default)]
    edition: Option<String>,
    #[serde(default)]
    seeders: Option<i64>,
    #[serde(default)]
    leechers: Option<i64>,
    #[serde(default)]
    approved: Option<bool>,
    #[serde(default)]
    rejected: Option<bool>,
    #[serde(default)]
    temporarily_rejected: Option<bool>,
    #[serde(default)]
    download_allowed: Option<bool>,
    #[serde(default)]
    rejections: Vec<String>,
}

impl ReleaseResource {
    /// An opaque handle for the release. Its guid can be an indexer download
    /// URL or magnet link carrying an API key or passkey, so it never leaves
    /// Omni; a grab finds the release again by this handle.
    fn release_id(&self) -> Option<String> {
        let digest = omni_core::digest::sha256_hex(format!(
            "{}\n{}",
            self.indexer_id?,
            self.guid.as_deref()?
        ));
        Some(digest.get(..32).unwrap_or(&digest).to_owned())
    }

    fn acceptable(&self) -> bool {
        self.approved == Some(true)
            && self.rejected != Some(true)
            && self.temporarily_rejected != Some(true)
            && self.download_allowed != Some(false)
    }
}

/// Why a search or grab was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchPurpose {
    /// The movie has no file yet.
    Missing,
    /// The movie's file is below its profile's cutoff.
    Upgrade,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovieSearchVia {
    Add,
    Search,
    Grab,
}

/// Durable per-movie search or grab reservation, keyed by TMDB id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieSearchData {
    pub tmdb_id: i64,
    pub state: SearchState,
    pub via: MovieSearchVia,
    pub requested_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<SearchPurpose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<i64>,
    /// The movie's file when the search or grab was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_file_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_title: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for MovieSearchData {
    const NAME: &'static str = "media-movie-search";
    type Key = String;
    fn key(&self) -> String {
        search_key(self.tmdb_id)
    }
}

fn search_key(tmdb_id: i64) -> String {
    tmdb_id.to_string()
}

fn ttl() -> UpsertOpts {
    UpsertOpts {
        ttl_ms: Some(SEARCH_RECORD_TTL_MS),
        ..UpsertOpts::default()
    }
}

/// The last search or grab Omni started or asked for.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastMovieSearch {
    pub requested_at: i64,
    pub via: MovieSearchVia,
    pub purpose: Option<SearchPurpose>,
    pub state: SearchState,
    pub command_id: Option<i64>,
    pub previous_file_id: Option<i64>,
    pub release_title: Option<String>,
}

impl From<&MovieSearchData> for LastMovieSearch {
    fn from(row: &MovieSearchData) -> Self {
        Self {
            requested_at: row.requested_at,
            via: row.via,
            purpose: row.purpose,
            state: row.state,
            command_id: row.command_id,
            previous_file_id: row.previous_file_id,
            release_title: row.release_title.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieFile {
    pub id: Option<i64>,
    pub relative_path: Option<String>,
    pub size_bytes: Option<f64>,
    pub date_added: Option<String>,
    pub quality: Option<String>,
    pub resolution: Option<i64>,
    pub source: Option<String>,
    pub video_resolution: Option<String>,
    pub video_codec: Option<String>,
    pub video_bit_depth: Option<i64>,
    pub dynamic_range: Option<String>,
    pub dynamic_range_type: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f64>,
    pub audio_languages: Option<String>,
    pub run_time: Option<String>,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub scene_name: Option<String>,
    pub edition: Option<String>,
    /// Present only with `detail`.
    pub custom_formats: Option<Vec<String>>,
    pub custom_format_score: Option<i64>,
    pub cutoff_not_met: Option<bool>,
}

impl From<&MovieFileResource> for MovieFile {
    fn from(f: &MovieFileResource) -> Self {
        let media = f.media_info.clone().unwrap_or_default();
        let quality = f.quality.as_ref().and_then(|q| q.quality.as_ref());
        Self {
            id: f.id,
            relative_path: f.relative_path.clone(),
            size_bytes: f.size,
            date_added: f.date_added.clone(),
            quality: quality.and_then(|q| q.name.clone()),
            resolution: quality.and_then(|q| q.resolution),
            source: quality.and_then(|q| q.source.clone()),
            video_resolution: media.resolution,
            video_codec: media.video_codec,
            video_bit_depth: media.video_bit_depth,
            dynamic_range: media.video_dynamic_range.filter(|s| !s.is_empty()),
            dynamic_range_type: media.video_dynamic_range_type.filter(|s| !s.is_empty()),
            audio_codec: media.audio_codec,
            audio_channels: media.audio_channels,
            audio_languages: media.audio_languages,
            run_time: media.run_time,
            languages: names(&f.languages),
            release_group: f.release_group.clone(),
            scene_name: f.scene_name.clone(),
            edition: f.edition.clone().filter(|e| !e.is_empty()),
            custom_formats: f.custom_formats.as_deref().map(names),
            custom_format_score: f.custom_format_score,
            cutoff_not_met: f.quality_cutoff_not_met,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedFile {
    pub id: Option<i64>,
    pub relative_path: Option<String>,
    pub size_bytes: Option<f64>,
    pub quality: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnmanagedFile {
    pub path: Option<String>,
    pub size_bytes: Option<f64>,
    pub quality: Option<String>,
    pub rejections: Vec<String>,
}

/// What sits in the movie's library folder.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryFiles {
    /// Files Radarr manages for the movie.
    pub managed: Vec<ManagedFile>,
    /// Other video files in the movie's folder; `null` when the folder could not be scanned.
    pub unmanaged: Option<Vec<UnmanagedFile>>,
    /// Exactly one managed file and a scanned folder with no other video file.
    pub exactly_one_file: bool,
}

impl LibraryFiles {
    fn new(managed: Vec<ManagedFile>, unmanaged: Option<Vec<UnmanagedFile>>) -> Self {
        let exactly_one_file = managed.len() == 1 && unmanaged.as_ref().is_some_and(Vec::is_empty);
        Self {
            managed,
            unmanaged,
            exactly_one_file,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    pub event_type: Option<String>,
    pub date: Option<String>,
    pub source_title: Option<String>,
    pub quality: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeState {
    /// No replacement has been grabbed or imported yet.
    Pending,
    /// A download for the movie is in Radarr's queue.
    Downloading,
    /// The file changed; the folder was not checked in this read (use `detail`).
    Imported,
    /// The file changed and it is the only video file in the library for the movie.
    Verified,
    /// The file changed but other video files remain in the library.
    ExtraFiles,
    /// The movie has no file at all now.
    FileMissing,
}

/// The outcome of the last upgrade Omni asked for.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeCheck {
    pub state: UpgradeState,
    pub previous_file_id: i64,
    pub current_file_id: Option<i64>,
}

/// The pure decision behind [`UpgradeCheck`].
pub fn upgrade_state(
    previous_file_id: i64,
    current_file_id: Option<i64>,
    downloading: bool,
    library: Option<&LibraryFiles>,
) -> UpgradeState {
    match current_file_id {
        None => UpgradeState::FileMissing,
        Some(current) if current == previous_file_id => {
            if downloading {
                UpgradeState::Downloading
            } else {
                UpgradeState::Pending
            }
        }
        Some(_) => match library {
            // Without a folder scan the change is real but unverified.
            None => UpgradeState::Imported,
            Some(files) if files.unmanaged.is_none() => UpgradeState::Imported,
            Some(files) if files.exactly_one_file => UpgradeState::Verified,
            Some(_) => UpgradeState::ExtraFiles,
        },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveMovieSearch {
    pub name: String,
    pub status: String,
}

/// A movie as Radarr tracks it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieStatus {
    pub radarr_id: i64,
    pub tmdb_id: i64,
    pub imdb_id: Option<String>,
    pub title: String,
    pub year: Option<i64>,
    pub collection: Option<String>,
    pub collection_tmdb_id: Option<i64>,
    pub monitored: bool,
    pub quality_profile_id: Option<i64>,
    pub quality_profile: Option<String>,
    pub upgrade_allowed: Option<bool>,
    pub has_file: bool,
    pub is_available: Option<bool>,
    pub path: Option<String>,
    pub file: Option<MovieFile>,
    /// Radarr's count of files it manages for the movie.
    pub managed_file_count: Option<i64>,
    pub initial_search_pending: bool,
    pub downloads: Vec<Download>,
    pub active_searches: Vec<ActiveMovieSearch>,
    pub last_search: Option<LastMovieSearch>,
    pub upgrade: Option<UpgradeCheck>,
    /// Present only with `detail`.
    pub library_files: Option<LibraryFiles>,
    /// Present only with `detail`: the latest Radarr history events, newest first.
    pub history: Option<Vec<HistoryEvent>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoviesQuery {
    pub tmdb_ids: Option<BTreeSet<i64>>,
    /// Case-insensitive substring of the title or the collection's title.
    pub query: Option<String>,
    /// Only movies with a file below their profile's cutoff.
    pub cutoff_unmet: bool,
    /// Only movies without a file.
    pub missing: bool,
    pub detail: bool,
    pub cursor: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoviesPage {
    pub items: Vec<MovieStatus>,
    pub next_cursor: Option<usize>,
    pub total: usize,
    /// Requested TMDB ids Radarr does not track.
    pub not_tracked: Vec<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MovieResult {
    Preview,
    Updated,
    Unchanged,
    Searched,
    NothingSearched,
    Listed,
    NotTracked,
    ProfileNotFound,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieSettings {
    pub quality_profile_id: Option<i64>,
    pub monitored: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileChange {
    pub tmdb_id: i64,
    pub radarr_id: i64,
    pub title: String,
    pub before: MovieSettings,
    /// Planned in a preview; Radarr's state read back otherwise.
    pub after: MovieSettings,
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileSetRequest {
    pub tmdb_ids: BTreeSet<i64>,
    pub quality_profile_id: i64,
    pub monitored: Option<bool>,
    pub dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSetOutcome {
    pub result: MovieResult,
    pub profile: Option<ProfileRef>,
    pub movies: Vec<ProfileChange>,
    pub not_tracked: Vec<i64>,
}

impl ProfileSetOutcome {
    fn of(result: MovieResult) -> Self {
        Self {
            result,
            profile: None,
            movies: Vec::new(),
            not_tracked: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MovieSearchOutcome {
    /// Radarr accepted the search now.
    Queued,
    /// The request may have reached Radarr; it is not retried.
    Uncertain,
    /// Radarr rejected the search; nothing was started.
    Failed,
    NotTracked,
    NotMonitored,
    /// `missing` was asked for but the movie has a file.
    HasFile,
    /// `upgrade` was asked for but the movie has no file.
    NoFile,
    /// The file already meets the profile's quality cutoff.
    CutoffMet,
    /// The movie's profile does not allow upgrades.
    UpgradesDisabled,
    /// Radarr's search after adding the movie has not run yet.
    InitialSearchPending,
    /// A Radarr search covering the movie is queued or running.
    AlreadyRunning,
    /// A download for the movie is in Radarr's queue.
    Downloading,
    /// Omni searched for the movie within the window.
    RecentlySearched,
    /// Omni or Radarr grabbed a release for the movie within the window.
    RecentlyGrabbed,
    /// Radarr's history for the movie could not be read; nothing was searched.
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieSearchResult {
    pub tmdb_id: i64,
    pub radarr_id: Option<i64>,
    pub title: Option<String>,
    pub outcome: MovieSearchOutcome,
    pub command_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MovieSearchRequest {
    pub tmdb_ids: BTreeSet<i64>,
    pub purpose: SearchPurpose,
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieSearchSummary {
    pub result: MovieResult,
    pub movies: Vec<MovieSearchResult>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    /// Omni's handle for the release; pass it to a grab.
    pub release_id: String,
    pub indexer_id: i64,
    pub indexer: Option<String>,
    pub title: Option<String>,
    pub size_bytes: Option<f64>,
    pub age_days: Option<i64>,
    pub protocol: Option<String>,
    pub quality: Option<String>,
    pub resolution: Option<i64>,
    pub custom_formats: Vec<String>,
    pub custom_format_score: Option<i64>,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub edition: Option<String>,
    pub seeders: Option<i64>,
    pub leechers: Option<i64>,
    /// Radarr would grab it under the movie's profile and release rules.
    pub approved: bool,
    pub rejections: Vec<String>,
}

impl Release {
    fn of(r: &ReleaseResource) -> Option<Self> {
        Some(Self {
            release_id: r.release_id()?,
            indexer_id: r.indexer_id?,
            indexer: r.indexer.clone(),
            title: r.title.as_deref().map(redact),
            size_bytes: r.size,
            age_days: r.age,
            protocol: r.protocol.clone(),
            quality: r.quality.as_ref().and_then(QualityModel::name),
            resolution: r.quality.as_ref().and_then(QualityModel::resolution),
            custom_formats: names(&r.custom_formats),
            custom_format_score: r.custom_format_score,
            languages: names(&r.languages),
            release_group: r.release_group.clone(),
            edition: r.edition.clone().filter(|e| !e.is_empty()),
            seeders: r.seeders,
            leechers: r.leechers,
            approved: r.acceptable(),
            rejections: r.rejections.iter().map(|s| redact(s)).collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleasesOutcome {
    pub result: MovieResult,
    pub movie: Option<MovieStatus>,
    /// Approved releases first, each group in Radarr's preference order.
    pub releases: Vec<Release>,
    pub total: usize,
    pub approved: usize,
}

impl ReleasesOutcome {
    fn of(result: MovieResult) -> Self {
        Self {
            result,
            movie: None,
            releases: Vec::new(),
            total: 0,
            approved: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrabRequest {
    pub tmdb_id: i64,
    /// A [`Release::release_id`] from a listing.
    pub release_id: String,
    pub force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrabOutcome {
    /// Radarr sent the release to the download client.
    Grabbed,
    /// The request may have reached Radarr; it is not retried.
    Uncertain,
    /// Radarr refused the grab; nothing was downloaded.
    Failed,
    NotTracked,
    /// The release is not in a fresh search for the movie.
    ReleaseNotFound,
    /// Radarr's profile or release rules reject the release.
    ReleaseRejected,
    InitialSearchPending,
    AlreadyRunning,
    Downloading,
    RecentlyGrabbed,
    /// An add's search, or a search whose outcome is uncertain, is within the window.
    RecentlySearched,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrabSummary {
    pub outcome: GrabOutcome,
    pub radarr_id: Option<i64>,
    pub title: Option<String>,
    pub release: Option<Release>,
}

impl GrabSummary {
    fn of(outcome: GrabOutcome) -> Self {
        Self {
            outcome,
            radarr_id: None,
            title: None,
            release: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionMovie {
    #[serde(default)]
    tmdb_id: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    is_existing: Option<bool>,
    #[serde(default)]
    is_excluded: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionResource {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tmdb_id: Option<i64>,
    #[serde(default)]
    monitored: Option<bool>,
    #[serde(default)]
    movies: Vec<CollectionMovie>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionMember {
    pub tmdb_id: Option<i64>,
    pub title: Option<String>,
    pub year: Option<i64>,
    /// Radarr tracks the movie.
    pub tracked: bool,
    /// On Radarr's import exclusion list.
    pub excluded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovieCollection {
    pub tmdb_id: Option<i64>,
    pub title: String,
    /// Radarr adds new members automatically.
    pub monitored: bool,
    pub tracked: usize,
    pub untracked: usize,
    pub members: Vec<CollectionMember>,
}

/// Why an operation stopped early.
type Step<T> = Result<T, InspectError>;

fn step<T>(result: HttpResult<T>) -> Step<T> {
    read(ArrService::Radarr, result)
}

fn store_failure(error: StoreError) {
    tracing::warn!(target: LOG, "Movie search record failed: {error}");
}

/// The common reads behind a movie operation.
struct Snapshot {
    movies: Vec<MovieResource>,
    profiles: Vec<QualityProfileResource>,
    queue: Vec<QueueRecord>,
    commands: Vec<CommandResource>,
}

impl Snapshot {
    fn by_tmdb(&self, tmdb_id: i64) -> Option<&MovieResource> {
        self.movies
            .iter()
            .find(|m| m.tmdb_id == Some(tmdb_id) && m.id.is_some())
    }

    fn profile(&self, id: Option<i64>) -> Option<&QualityProfileResource> {
        self.profiles.iter().find(|p| Some(p.id()) == id)
    }

    fn downloads(&self, movie_id: i64) -> Vec<Download> {
        self.queue
            .iter()
            .filter(|r| r.movie_id == Some(movie_id))
            .map(Download::from)
            .collect()
    }

    fn searching(&self, movie_id: i64) -> Vec<ActiveMovieSearch> {
        self.commands
            .iter()
            .filter(|c| c.searches(movie_id))
            .map(|c| ActiveMovieSearch {
                name: c.name.clone().unwrap_or_default(),
                status: c.status.clone().unwrap_or_default(),
            })
            .collect()
    }
}

/// The reason a search or grab must not run now, before any record check.
fn blocked(snapshot: &Snapshot, movie: &MovieResource, id: i64) -> Option<MovieSearchOutcome> {
    if movie.initial_search_pending() {
        Some(MovieSearchOutcome::InitialSearchPending)
    } else if !snapshot.downloads(id).is_empty() {
        Some(MovieSearchOutcome::Downloading)
    } else if !snapshot.searching(id).is_empty() {
        Some(MovieSearchOutcome::AlreadyRunning)
    } else {
        None
    }
}

/// Why `purpose` does not apply to the movie, if it does not.
fn purpose_mismatch(
    purpose: SearchPurpose,
    movie: &MovieResource,
    profile: Option<&QualityProfileResource>,
) -> Option<MovieSearchOutcome> {
    if movie.monitored != Some(true) {
        return Some(MovieSearchOutcome::NotMonitored);
    }
    let has_file = movie.has_file == Some(true);
    match purpose {
        SearchPurpose::Missing if has_file => Some(MovieSearchOutcome::HasFile),
        SearchPurpose::Missing => None,
        SearchPurpose::Upgrade if !has_file => Some(MovieSearchOutcome::NoFile),
        SearchPurpose::Upgrade if profile.is_some_and(|p| !p.upgrade_allowed()) => {
            Some(MovieSearchOutcome::UpgradesDisabled)
        }
        SearchPurpose::Upgrade
            if movie
                .movie_file
                .as_ref()
                .and_then(|f| f.quality_cutoff_not_met)
                == Some(false) =>
        {
            Some(MovieSearchOutcome::CutoffMet)
        }
        SearchPurpose::Upgrade => None,
    }
}

/// Movie operations over the configured Radarr. Mutations are serialized
/// and run to completion even when the calling request is dropped.
#[derive(Clone)]
pub struct RadarrMovies {
    http: ArrHttp,
    config: ArrConfig,
    store: Store,
    tracker: TaskTracker,
    writes: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl RadarrMovies {
    pub fn new(http: ArrHttp, config: ArrConfig, store: Store, tracker: TaskTracker) -> Self {
        Self {
            http,
            config,
            store,
            tracker,
            writes: std::sync::Arc::default(),
        }
    }

    fn connection(&self) -> Step<(&str, &str)> {
        self.config
            .connection()
            .ok_or(InspectError::NotConfigured("Radarr"))
    }

    fn now(&self) -> i64 {
        self.store.clock().now_ms()
    }

    /// Movies with their files, profiles, downloads and search state.
    pub async fn get(&self, query: MoviesQuery) -> Step<MoviesPage> {
        let connection = self.connection()?;
        let snapshot = self.snapshot(connection).await?;
        let needle = query
            .query
            .as_deref()
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let mut selected: Vec<&MovieResource> = snapshot
            .movies
            .iter()
            .filter(|m| m.id.is_some() && m.tmdb_id.is_some())
            .filter(|m| {
                query
                    .tmdb_ids
                    .as_ref()
                    .is_none_or(|ids| m.tmdb_id.is_some_and(|t| ids.contains(&t)))
            })
            .filter(|m| {
                needle.as_deref().is_none_or(|needle| {
                    let in_title = m
                        .title
                        .as_deref()
                        .is_some_and(|t| t.to_lowercase().contains(needle));
                    let in_collection = m
                        .collection
                        .as_ref()
                        .and_then(|c| c.title.as_deref())
                        .is_some_and(|t| t.to_lowercase().contains(needle));
                    in_title || in_collection
                })
            })
            .filter(|m| !query.missing || m.has_file != Some(true))
            .filter(|m| {
                !query.cutoff_unmet
                    || (m.has_file == Some(true)
                        && m.movie_file.as_ref().and_then(|f| f.quality_cutoff_not_met)
                            == Some(true))
            })
            .collect();
        selected.sort_by(|a, b| {
            let key = |m: &MovieResource| {
                (
                    m.collection
                        .as_ref()
                        .and_then(|c| c.title.clone())
                        .unwrap_or_default()
                        .to_lowercase(),
                    m.year.unwrap_or_default(),
                    m.title.clone().unwrap_or_default().to_lowercase(),
                )
            };
            key(a).cmp(&key(b))
        });
        let not_tracked = query
            .tmdb_ids
            .as_ref()
            .map(|ids| {
                ids.iter()
                    .copied()
                    .filter(|id| snapshot.by_tmdb(*id).is_none())
                    .collect()
            })
            .unwrap_or_default();
        let total = selected.len();
        let page: Vec<&MovieResource> = selected
            .into_iter()
            .skip(query.cursor)
            .take(query.limit)
            .collect();
        let next = query.cursor.saturating_add(page.len());
        let records = self
            .records(page.iter().filter_map(|m| m.tmdb_id).collect())
            .await?;
        let mut items = Vec::with_capacity(page.len());
        for chunk in page.chunks(DETAIL_CONCURRENCY) {
            let statuses = futures::future::join_all(chunk.iter().map(|movie| {
                self.movie_status(connection, &snapshot, movie, &records, query.detail)
            }))
            .await;
            for status in statuses {
                items.push(status?);
            }
        }
        Ok(MoviesPage {
            items,
            next_cursor: (next < total).then_some(next),
            total,
            not_tracked,
        })
    }

    /// Changes the quality profile (and optionally monitoring) of the listed
    /// movies through Radarr's editor, then reads each one back. Never searches.
    pub async fn set_profile(&self, request: ProfileSetRequest) -> ProfileSetOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.set_profile_inner(request)
                .await
                .unwrap_or_else(|error| ProfileSetOutcome::of(failure(&error)))
        })
        .await
    }

    pub async fn search(&self, request: MovieSearchRequest) -> MovieSearchSummary {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.search_inner(request)
                .await
                .unwrap_or_else(|error| MovieSearchSummary {
                    result: failure(&error),
                    movies: Vec::new(),
                })
        })
        .await
    }

    /// Radarr's interactive search: every release its indexers return for
    /// the movie, with Radarr's verdict. Grabs nothing.
    pub async fn releases(&self, tmdb_id: i64) -> ReleasesOutcome {
        self.releases_inner(tmdb_id)
            .await
            .unwrap_or_else(|error| ReleasesOutcome::of(failure(&error)))
    }

    /// The indexer search that finds the release runs outside the write lock
    /// (it can take minutes); the checks, reservation and grab run inside it.
    pub async fn grab(&self, request: GrabRequest) -> GrabSummary {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let failed = |error: InspectError| {
                GrabSummary::of(match failure(&error) {
                    MovieResult::Unavailable => GrabOutcome::Unavailable,
                    _ => GrabOutcome::Error,
                })
            };
            let prepared = match this.prepare_grab(&request).await {
                Ok(Prepared::Ready(prepared)) => prepared,
                Ok(Prepared::Done(summary)) => return *summary,
                Err(error) => return failed(error),
            };
            let _writes = this.writes.lock().await;
            this.commit_grab(&request, *prepared)
                .await
                .unwrap_or_else(failed)
        })
        .await
    }

    /// Radarr's collections whose title, or a member's title, contains `query`.
    pub async fn collections(&self, query: Option<&str>) -> Step<Vec<MovieCollection>> {
        let connection = self.connection()?;
        let all = step(
            self.http
                .send_limited::<Vec<CollectionResource>>(
                    connection,
                    Method::GET,
                    "collection",
                    None,
                    Limits::LIBRARY,
                )
                .await,
        )?;
        let needle = query
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let mut collections: Vec<MovieCollection> = all
            .into_iter()
            .filter(|c| {
                needle.as_deref().is_none_or(|needle| {
                    let hit = |t: &Option<String>| {
                        t.as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(needle))
                    };
                    hit(&c.title) || c.movies.iter().any(|m| hit(&m.title))
                })
            })
            .map(|c| {
                let mut members: Vec<CollectionMember> = c
                    .movies
                    .iter()
                    .map(|m| CollectionMember {
                        tmdb_id: m.tmdb_id,
                        title: m.title.clone(),
                        year: m.year,
                        tracked: m.is_existing == Some(true),
                        excluded: m.is_excluded == Some(true),
                    })
                    .collect();
                members.sort_by_key(|m| (m.year.unwrap_or(i64::MAX), m.title.clone()));
                let tracked = members.iter().filter(|m| m.tracked).count();
                MovieCollection {
                    tmdb_id: c.tmdb_id,
                    title: c.title.unwrap_or_default(),
                    monitored: c.monitored == Some(true),
                    tracked,
                    untracked: members.len() - tracked,
                    members,
                }
            })
            .collect();
        collections.sort_by_key(|c| c.title.to_lowercase());
        Ok(collections)
    }

    /// A movie add (`media_watchlist_add` or a recommendation) with its
    /// post-add search recorded, serialized with the movie tools.
    pub async fn add_movie(&self, tmdb_id: i64) -> AddToWatchlistResult {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            add_radarr_movie_with(&this.http, &this.config, tmdb_id, Some(&this)).await
        })
        .await
    }

    async fn snapshot(&self, connection: (&str, &str)) -> Step<Snapshot> {
        let (movies, profiles, queue, commands) = futures::join!(
            self.http.send_limited::<Vec<MovieResource>>(
                connection,
                Method::GET,
                "movie",
                None,
                Limits::LIBRARY
            ),
            fetch_profiles(&self.http, ArrService::Radarr, connection),
            fetch_queue(&self.http, ArrService::Radarr, connection),
            self.http
                .request_json::<Vec<CommandResource>>(connection, "command", None),
        );
        Ok(Snapshot {
            movies: step(movies)?,
            profiles: profiles?,
            queue: queue?,
            commands: step(commands)?,
        })
    }

    async fn records(&self, tmdb_ids: Vec<i64>) -> Step<BTreeMap<i64, MovieSearchData>> {
        self.store
            .read(move |docs| {
                let mut rows = BTreeMap::new();
                for tmdb_id in tmdb_ids {
                    if let Some(row) = docs.get::<MovieSearchData>(&search_key(tmdb_id))? {
                        rows.insert(tmdb_id, row);
                    }
                }
                Ok(rows)
            })
            .await
            .map_err(|error| {
                store_failure(error);
                InspectError::Unavailable("Omni's search records")
            })
    }

    async fn movie_status(
        &self,
        connection: (&str, &str),
        snapshot: &Snapshot,
        movie: &MovieResource,
        records: &BTreeMap<i64, MovieSearchData>,
        detail: bool,
    ) -> Step<MovieStatus> {
        let id = movie.id.unwrap_or_default();
        let tmdb_id = movie.tmdb_id.unwrap_or_default();
        let profile = snapshot.profile(movie.quality_profile_id);
        let downloads = snapshot.downloads(id);
        let (files, history) = if detail {
            let (files, history) = futures::join!(
                self.library_files(connection, movie),
                self.history(connection, id)
            );
            // A failed detail read leaves that part null for this movie only.
            (files.ok(), history.ok())
        } else {
            (None, None)
        };
        let mut file = movie
            .movie_file
            .as_ref()
            .filter(|_| movie.has_file == Some(true));
        // `moviefile` carries the custom formats the movie list leaves out.
        let detailed = files.as_ref().and_then(|(_, raw)| {
            raw.iter()
                .find(|f| f.id.is_some() && f.id == file.and_then(|f| f.id))
        });
        if detailed.is_some() {
            file = detailed;
        }
        let library_files = files.as_ref().map(|(library, _)| library.clone());
        let record = records.get(&tmdb_id);
        let upgrade = record.and_then(|row| {
            let previous = row.previous_file_id?;
            (row.purpose == Some(SearchPurpose::Upgrade) || row.via == MovieSearchVia::Grab).then(
                || UpgradeCheck {
                    state: upgrade_state(
                        previous,
                        movie.file_id(),
                        !downloads.is_empty(),
                        library_files.as_ref(),
                    ),
                    previous_file_id: previous,
                    current_file_id: movie.file_id(),
                },
            )
        });
        Ok(MovieStatus {
            radarr_id: id,
            tmdb_id,
            imdb_id: movie.imdb_id.clone(),
            title: movie.title.clone().unwrap_or_default(),
            year: movie.year,
            collection: movie.collection.as_ref().and_then(|c| c.title.clone()),
            collection_tmdb_id: movie.collection.as_ref().and_then(|c| c.tmdb_id),
            monitored: movie.monitored == Some(true),
            quality_profile_id: movie.quality_profile_id,
            quality_profile: profile.map(QualityProfileResource::name),
            upgrade_allowed: profile.map(QualityProfileResource::upgrade_allowed),
            has_file: movie.has_file == Some(true),
            is_available: movie.is_available,
            path: movie.path.clone(),
            file: file.map(MovieFile::from),
            managed_file_count: movie.statistics.as_ref().and_then(|s| s.movie_file_count),
            initial_search_pending: movie.initial_search_pending(),
            downloads,
            active_searches: snapshot.searching(id),
            last_search: record.map(LastMovieSearch::from),
            upgrade,
            library_files,
            history,
        })
    }

    /// Radarr's managed files for the movie plus a scan of its folder for any
    /// other video file.
    async fn library_files(
        &self,
        connection: (&str, &str),
        movie: &MovieResource,
    ) -> Step<(LibraryFiles, Vec<MovieFileResource>)> {
        let id = movie.id.unwrap_or_default();
        let files_path = format!("moviefile?movieId={id}");
        let managed = step(
            self.http
                .request_json::<Vec<MovieFileResource>>(connection, &files_path, None)
                .await,
        )?;
        let unmanaged = match movie.path.as_deref().filter(|p| !p.is_empty()) {
            Some(folder) => {
                let scan_path = format!(
                    "manualimport?folder={}&filterExistingFiles=true",
                    omni_core::js::encode_uri_component(folder)
                );
                match self
                    .http
                    .send_limited::<Vec<ManualImportResource>>(
                        connection,
                        Method::GET,
                        &scan_path,
                        None,
                        Limits {
                            timeout: Duration::from_secs(60),
                            max_bytes: super::ARR_JSON_MAX_BYTES,
                        },
                    )
                    .await
                {
                    HttpResult::Ok(items) => Some(
                        items
                            .iter()
                            .map(|item| UnmanagedFile {
                                path: item.path.clone().or_else(|| item.relative_path.clone()),
                                size_bytes: item.size,
                                quality: item.quality.as_ref().and_then(QualityModel::name),
                                rejections: item
                                    .rejections
                                    .iter()
                                    .filter_map(|r| r.reason.clone())
                                    .collect(),
                            })
                            .collect(),
                    ),
                    _ => None,
                }
            }
            None => None,
        };
        let library = LibraryFiles::new(
            managed
                .iter()
                .map(|f| ManagedFile {
                    id: f.id,
                    relative_path: f.relative_path.clone(),
                    size_bytes: f.size,
                    quality: f.quality.as_ref().and_then(QualityModel::name),
                })
                .collect(),
            unmanaged,
        );
        Ok((library, managed))
    }

    async fn history_rows(
        &self,
        connection: (&str, &str),
        movie_id: i64,
    ) -> Step<Vec<HistoryResource>> {
        let path = format!("history/movie?movieId={movie_id}");
        let mut rows = step(
            self.http
                .request_json::<Vec<HistoryResource>>(connection, &path, None)
                .await,
        )?;
        rows.sort_by_key(|r| std::cmp::Reverse(r.at_ms()));
        Ok(rows)
    }

    async fn history(&self, connection: (&str, &str), movie_id: i64) -> Step<Vec<HistoryEvent>> {
        Ok(self
            .history_rows(connection, movie_id)
            .await?
            .iter()
            .take(10)
            .map(|r| HistoryEvent {
                event_type: r.event_type.clone(),
                date: r.date.clone(),
                source_title: r.source_title.as_deref().map(redact),
                quality: r.quality.as_ref().and_then(QualityModel::name),
            })
            .collect())
    }

    /// A grab in Radarr's history within the window (RSS, another client or the UI).
    async fn grabbed_recently(&self, connection: (&str, &str), movie_id: i64) -> Step<bool> {
        let since = self.now() - SEARCH_WINDOW_MS;
        Ok(self
            .history_rows(connection, movie_id)
            .await?
            .iter()
            // A grab whose date does not parse counts as recent.
            .any(|r| {
                r.event_type.as_deref() == Some("grabbed") && r.at_ms().is_none_or(|at| at >= since)
            }))
    }

    async fn set_profile_inner(&self, request: ProfileSetRequest) -> Step<ProfileSetOutcome> {
        let connection = self.connection()?;
        let snapshot = self.snapshot(connection).await?;
        let Some(profile) = snapshot.profile(Some(request.quality_profile_id)) else {
            return Ok(ProfileSetOutcome::of(MovieResult::ProfileNotFound));
        };
        let profile = ProfileRef {
            id: profile.id(),
            name: profile.name(),
        };
        let not_tracked: Vec<i64> = request
            .tmdb_ids
            .iter()
            .copied()
            .filter(|id| snapshot.by_tmdb(*id).is_none())
            .collect();
        let movies: Vec<&MovieResource> = request
            .tmdb_ids
            .iter()
            .filter_map(|id| snapshot.by_tmdb(*id))
            .collect();
        if movies.is_empty() {
            return Ok(ProfileSetOutcome {
                not_tracked,
                profile: Some(profile),
                ..ProfileSetOutcome::of(MovieResult::NotTracked)
            });
        }
        let settings = |m: &MovieResource| MovieSettings {
            quality_profile_id: m.quality_profile_id,
            monitored: m.monitored == Some(true),
        };
        let planned: Vec<ProfileChange> = movies
            .iter()
            .map(|m| {
                let before = settings(m);
                let after = MovieSettings {
                    quality_profile_id: Some(request.quality_profile_id),
                    monitored: request.monitored.unwrap_or(before.monitored),
                };
                ProfileChange {
                    tmdb_id: m.tmdb_id.unwrap_or_default(),
                    radarr_id: m.id.unwrap_or_default(),
                    title: m.title.clone().unwrap_or_default(),
                    changed: before != after,
                    before,
                    after,
                }
            })
            .collect();
        let changed: Vec<i64> = planned
            .iter()
            .filter(|c| c.changed)
            .map(|c| c.radarr_id)
            .collect();
        let outcome = |result, movies| ProfileSetOutcome {
            result,
            profile: Some(profile.clone()),
            movies,
            not_tracked: not_tracked.clone(),
        };
        if request.dry_run {
            return Ok(outcome(MovieResult::Preview, planned));
        }
        if changed.is_empty() {
            return Ok(outcome(MovieResult::Unchanged, planned));
        }
        let mut body = serde_json::json!({
            "movieIds": changed,
            "qualityProfileId": request.quality_profile_id,
            "moveFiles": false,
        });
        if let Some(monitored) = request.monitored {
            body["monitored"] = monitored.into();
        }
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, "movie/editor", body);
            return Ok(outcome(MovieResult::Updated, planned));
        }
        let sent = self
            .http
            .send_json::<Value>(connection, Method::PUT, "movie/editor", Some(body))
            .await;
        if WriteOutcome::of(&sent) == WriteOutcome::Rejected {
            tracing::warn!(target: LOG, "Radarr rejected a profile change for {changed:?}");
            return Ok(outcome(MovieResult::Error, planned));
        }
        // Accepted or uncertain alike, only Radarr's own state settles it.
        let mut verified = Vec::new();
        let mut all_match = true;
        for change in planned {
            if !change.changed {
                verified.push(change);
                continue;
            }
            let written = step(
                self.http
                    .request_json::<MovieResource>(
                        connection,
                        &format!("movie/{}", change.radarr_id),
                        None,
                    )
                    .await,
            )?;
            let after = settings(&written);
            all_match &= after == change.after;
            verified.push(ProfileChange { after, ..change });
        }
        tracing::info!(
            target: LOG,
            "Set quality profile {} on Radarr movies {changed:?} (verified: {all_match})",
            request.quality_profile_id
        );
        Ok(outcome(
            if all_match {
                MovieResult::Updated
            } else {
                MovieResult::Error
            },
            verified,
        ))
    }

    async fn search_inner(&self, request: MovieSearchRequest) -> Step<MovieSearchSummary> {
        let connection = self.connection()?;
        let snapshot = self.snapshot(connection).await?;
        let mut results = Vec::new();
        for &tmdb_id in &request.tmdb_ids {
            let Some(movie) = snapshot.by_tmdb(tmdb_id) else {
                results.push(MovieSearchResult {
                    tmdb_id,
                    radarr_id: None,
                    title: None,
                    outcome: MovieSearchOutcome::NotTracked,
                    command_id: None,
                });
                continue;
            };
            let id = movie.id.unwrap_or_default();
            let skip = match purpose_mismatch(
                request.purpose,
                movie,
                snapshot.profile(movie.quality_profile_id),
            )
            .or_else(|| blocked(&snapshot, movie, id))
            {
                Some(skip) => Some(skip),
                // A failed read skips only this movie, so the outcomes of
                // searches already sent in this call are still reported.
                None if !request.force => match self.grabbed_recently(connection, id).await {
                    Ok(true) => Some(MovieSearchOutcome::RecentlyGrabbed),
                    Ok(false) => None,
                    Err(_) => Some(MovieSearchOutcome::Unavailable),
                },
                None => None,
            };
            let (outcome, command_id) = match skip {
                Some(outcome) => (outcome, None),
                None => {
                    self.search_movie(connection, movie, request.purpose, request.force)
                        .await
                }
            };
            results.push(MovieSearchResult {
                tmdb_id,
                radarr_id: Some(id),
                title: movie.title.clone(),
                outcome,
                command_id,
            });
        }
        let started = results.iter().any(|r| {
            matches!(
                r.outcome,
                MovieSearchOutcome::Queued | MovieSearchOutcome::Uncertain
            )
        });
        Ok(MovieSearchSummary {
            result: if started {
                MovieResult::Searched
            } else {
                MovieResult::NothingSearched
            },
            movies: results,
        })
    }

    /// Reserve → command → record, exactly like a Sonarr season search: a
    /// reservation inside the window blocks the search unless forced, an
    /// uncertain request keeps its reservation and a rejected one restores the
    /// record it replaced.
    async fn search_movie(
        &self,
        connection: (&str, &str),
        movie: &MovieResource,
        purpose: SearchPurpose,
        force: bool,
    ) -> (MovieSearchOutcome, Option<i64>) {
        let (tmdb_id, id) = (
            movie.tmdb_id.unwrap_or_default(),
            movie.id.unwrap_or_default(),
        );
        let previous_file_id = movie.file_id();
        let row = move |now| MovieSearchData {
            tmdb_id,
            state: SearchState::Reserved,
            via: MovieSearchVia::Search,
            requested_at: now,
            purpose: Some(purpose),
            command_id: None,
            previous_file_id,
            release_title: None,
            extra: Extra::default(),
        };
        let previous = match self.reserve(tmdb_id, force, row).await {
            Reserved::Fresh(previous) => previous,
            Reserved::Blocked(via) => return (recent(via), None),
            Reserved::Failed => return (MovieSearchOutcome::Failed, None),
        };
        let body = serde_json::json!({ "name": "MoviesSearch", "movieIds": [id] });
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, "command", body);
            return (MovieSearchOutcome::Queued, None);
        }
        let sent = self
            .http
            .request_json::<CommandResource>(connection, "command", Some(body))
            .await;
        let written = WriteOutcome::of(&sent);
        let command_id = match &sent {
            HttpResult::Ok(command) => command.id,
            _ => None,
        };
        self.settle(tmdb_id, previous, written, command_id).await;
        let outcome = match written {
            WriteOutcome::Accepted => {
                tracing::info!(
                    target: LOG,
                    "Started a {purpose:?} search of movie tmdb:{tmdb_id}"
                );
                MovieSearchOutcome::Queued
            }
            WriteOutcome::Uncertain => MovieSearchOutcome::Uncertain,
            WriteOutcome::Rejected => {
                tracing::warn!(target: LOG, "Radarr rejected a search of tmdb:{tmdb_id}");
                MovieSearchOutcome::Failed
            }
        };
        (outcome, command_id)
    }

    async fn releases_inner(&self, tmdb_id: i64) -> Step<ReleasesOutcome> {
        let connection = self.connection()?;
        let snapshot = self.snapshot(connection).await?;
        let Some(movie) = snapshot.by_tmdb(tmdb_id) else {
            return Ok(ReleasesOutcome::of(MovieResult::NotTracked));
        };
        let raw = self.fetch_releases(connection, movie).await?;
        let records = self.records(vec![tmdb_id]).await?;
        let status = self
            .movie_status(connection, &snapshot, movie, &records, false)
            .await?;
        let (mut approved, rejected): (Vec<Release>, Vec<Release>) =
            raw.iter().filter_map(Release::of).partition(|r| r.approved);
        let (total, approved_count) = (approved.len() + rejected.len(), approved.len());
        approved.extend(rejected);
        approved.truncate(RELEASE_LIMIT);
        Ok(ReleasesOutcome {
            result: MovieResult::Listed,
            movie: Some(status),
            releases: approved,
            total,
            approved: approved_count,
        })
    }

    async fn fetch_releases(
        &self,
        connection: (&str, &str),
        movie: &MovieResource,
    ) -> Step<Vec<ReleaseResource>> {
        let path = format!("release?movieId={}", movie.id.unwrap_or_default());
        step(
            self.http
                .send_limited::<Vec<ReleaseResource>>(
                    connection,
                    Method::GET,
                    &path,
                    None,
                    Limits::INDEXER_SEARCH,
                )
                .await,
        )
    }

    /// Everything before the grab that writes nothing: the early checks and
    /// a fresh indexer search that must still offer the release, approved.
    async fn prepare_grab(&self, request: &GrabRequest) -> Step<Prepared> {
        let connection = self.connection()?;
        let snapshot = self.snapshot(connection).await?;
        let Some(movie) = snapshot.by_tmdb(request.tmdb_id) else {
            return Ok(Prepared::Done(Box::new(GrabSummary::of(
                GrabOutcome::NotTracked,
            ))));
        };
        let summary = |outcome, release| {
            Prepared::Done(Box::new(GrabSummary {
                outcome,
                radarr_id: movie.id,
                title: movie.title.clone(),
                release,
            }))
        };
        if let Some(blocked) = self
            .grab_blocked(connection, &snapshot, movie, request.force)
            .await?
        {
            return Ok(summary(blocked, None));
        }
        // A fresh search proves the release is still offered and still
        // acceptable, and refreshes Radarr's release cache for the grab.
        let raw = self.fetch_releases(connection, movie).await?;
        let Some(found) = raw
            .into_iter()
            .find(|r| r.release_id().as_deref() == Some(request.release_id.as_str()))
        else {
            return Ok(summary(GrabOutcome::ReleaseNotFound, None));
        };
        if !found.acceptable() {
            return Ok(summary(GrabOutcome::ReleaseRejected, Release::of(&found)));
        }
        Ok(Prepared::Ready(Box::new(PreparedGrab {
            movie: movie.clone(),
            found,
        })))
    }

    async fn commit_grab(
        &self,
        request: &GrabRequest,
        prepared: PreparedGrab,
    ) -> Step<GrabSummary> {
        let connection = self.connection()?;
        let PreparedGrab { movie, found } = prepared;
        let id = movie.id.unwrap_or_default();
        let release = Release::of(&found);
        let summary = |outcome, release| GrabSummary {
            outcome,
            radarr_id: Some(id),
            title: movie.title.clone(),
            release,
        };
        // The indexer search can take minutes: RSS, the UI or another client
        // may have grabbed for the movie meanwhile, so look again.
        let current = step(
            self.http
                .request_json::<MovieResource>(connection, &format!("movie/{id}"), None)
                .await,
        )?;
        let (queue, commands) = futures::join!(
            fetch_queue(&self.http, ArrService::Radarr, connection),
            self.http
                .request_json::<Vec<CommandResource>>(connection, "command", None),
        );
        let fresh = Snapshot {
            movies: Vec::new(),
            profiles: Vec::new(),
            queue: queue?,
            commands: step(commands)?,
        };
        if let Some(blocked) = self
            .grab_blocked(connection, &fresh, &current, request.force)
            .await?
        {
            return Ok(summary(blocked, release));
        }
        let (Some(guid), Some(indexer_id)) = (found.guid.clone(), found.indexer_id) else {
            return Ok(summary(GrabOutcome::ReleaseNotFound, None));
        };
        let tmdb_id = request.tmdb_id;
        let release_title = found.title.as_deref().map(redact);
        let previous_file_id = current.file_id();
        let purpose = if current.has_file == Some(true) {
            SearchPurpose::Upgrade
        } else {
            SearchPurpose::Missing
        };
        let row = move |now| MovieSearchData {
            tmdb_id,
            state: SearchState::Reserved,
            via: MovieSearchVia::Grab,
            requested_at: now,
            purpose: Some(purpose),
            command_id: None,
            previous_file_id,
            release_title,
            extra: Extra::default(),
        };
        let previous = match self.reserve_grab(tmdb_id, request.force, row).await {
            Reserved::Fresh(previous) => previous,
            Reserved::Blocked(MovieSearchVia::Grab) => {
                return Ok(summary(GrabOutcome::RecentlyGrabbed, release));
            }
            Reserved::Blocked(_) => return Ok(summary(GrabOutcome::RecentlySearched, release)),
            Reserved::Failed => return Ok(summary(GrabOutcome::Error, release)),
        };
        let body = serde_json::json!({
            "guid": guid,
            "indexerId": indexer_id,
            "movieId": id,
        });
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, "release", body);
            return Ok(summary(GrabOutcome::Grabbed, release));
        }
        let sent = self
            .http
            .send_limited::<Value>(connection, Method::POST, "release", Some(body), GRAB_LIMITS)
            .await;
        let written = WriteOutcome::of(&sent);
        self.settle(tmdb_id, previous, written, None).await;
        let outcome = match written {
            WriteOutcome::Accepted => {
                tracing::info!(target: LOG, "Grabbed a chosen release for movie tmdb:{tmdb_id}");
                GrabOutcome::Grabbed
            }
            WriteOutcome::Uncertain => GrabOutcome::Uncertain,
            WriteOutcome::Rejected => {
                tracing::warn!(target: LOG, "Radarr refused a chosen release for tmdb:{tmdb_id}");
                GrabOutcome::Failed
            }
        };
        Ok(summary(outcome, release))
    }

    /// Why a grab must not run now: Radarr's own activity, then (unless
    /// forced) a grab in Radarr's history within the window.
    async fn grab_blocked(
        &self,
        connection: (&str, &str),
        snapshot: &Snapshot,
        movie: &MovieResource,
        force: bool,
    ) -> Step<Option<GrabOutcome>> {
        let id = movie.id.unwrap_or_default();
        if let Some(blocked) = blocked(snapshot, movie, id) {
            return Ok(Some(match blocked {
                MovieSearchOutcome::InitialSearchPending => GrabOutcome::InitialSearchPending,
                MovieSearchOutcome::Downloading => GrabOutcome::Downloading,
                _ => GrabOutcome::AlreadyRunning,
            }));
        }
        if !force && self.grabbed_recently(connection, id).await? {
            return Ok(Some(GrabOutcome::RecentlyGrabbed));
        }
        Ok(None)
    }

    /// Writes `row(now)` unless a record inside the window blocks it.
    async fn reserve(
        &self,
        tmdb_id: i64,
        force: bool,
        row: impl FnOnce(i64) -> MovieSearchData + Send + 'static,
    ) -> Reserved {
        self.reserve_where(tmdb_id, force, |_| true, row).await
    }

    /// [`Self::reserve`] for a grab. A search Radarr accepted does not block
    /// it (a search that found nothing acceptable is why a release is picked
    /// by hand), but an earlier grab, an add's pending search, or a search
    /// that may still be reaching Radarr does.
    async fn reserve_grab(
        &self,
        tmdb_id: i64,
        force: bool,
        row: impl FnOnce(i64) -> MovieSearchData + Send + 'static,
    ) -> Reserved {
        let blocks = |r: &MovieSearchData| match r.via {
            MovieSearchVia::Grab | MovieSearchVia::Add => true,
            MovieSearchVia::Search => {
                matches!(r.state, SearchState::Reserved | SearchState::Uncertain)
            }
        };
        self.reserve_where(tmdb_id, force, blocks, row).await
    }

    async fn reserve_where(
        &self,
        tmdb_id: i64,
        force: bool,
        blocks: impl Fn(&MovieSearchData) -> bool + Send + 'static,
        row: impl FnOnce(i64) -> MovieSearchData + Send + 'static,
    ) -> Reserved {
        let record = self.http.mode() == SideEffectMode::Record;
        let reserved = self
            .store
            .write(move |tx| {
                let now = tx.now_ms();
                let previous = tx.get::<MovieSearchData>(&search_key(tmdb_id))?;
                if let Some(blocking) = previous
                    .as_ref()
                    .filter(|p| !force && now - p.requested_at < SEARCH_WINDOW_MS && blocks(p))
                {
                    return Ok::<_, StoreError>(Err(blocking.via));
                }
                if !record {
                    tx.upsert(&row(now), ttl())?;
                }
                Ok(Ok(previous))
            })
            .await;
        match reserved {
            Ok(Ok(previous)) => Reserved::Fresh(previous),
            Ok(Err(via)) => Reserved::Blocked(via),
            Err(error) => {
                store_failure(error);
                Reserved::Failed
            }
        }
    }

    async fn settle(
        &self,
        tmdb_id: i64,
        previous: Option<MovieSearchData>,
        written: WriteOutcome,
        command_id: Option<i64>,
    ) {
        let state = match written {
            WriteOutcome::Accepted => SearchState::Queued,
            WriteOutcome::Uncertain => SearchState::Uncertain,
            WriteOutcome::Rejected => SearchState::Reserved,
        };
        let settled = self
            .store
            .write(move |tx| {
                let key = search_key(tmdb_id);
                if written == WriteOutcome::Rejected {
                    return match previous {
                        Some(previous) => tx.upsert(&previous, ttl()),
                        None => tx.delete::<MovieSearchData>(&key).map(|_| ()),
                    };
                }
                tx.update::<MovieSearchData>(
                    &key,
                    |row| MovieSearchData {
                        state,
                        command_id,
                        ..row
                    },
                    Default::default(),
                )
                .map(|_| ())
            })
            .await;
        if let Err(error) = settled {
            // The reservation stays and keeps blocking a repeat.
            store_failure(error);
        }
    }

    /// The add's reservation; returns the record it replaced.
    async fn reserve_add(&self, tmdb_id: i64) -> Result<Option<MovieSearchData>, StoreError> {
        self.store
            .write(move |tx| {
                let now = tx.now_ms();
                let previous = tx.get::<MovieSearchData>(&search_key(tmdb_id))?;
                tx.upsert(
                    &MovieSearchData {
                        tmdb_id,
                        state: SearchState::Reserved,
                        via: MovieSearchVia::Add,
                        requested_at: now,
                        purpose: Some(SearchPurpose::Missing),
                        command_id: None,
                        previous_file_id: None,
                        release_title: None,
                        extra: Extra::default(),
                    },
                    ttl(),
                )?;
                Ok(previous)
            })
            .await
    }

    /// A rejected add restores the record its reservation replaced (or
    /// releases it); otherwise the reservation stays.
    async fn settle_add(&self, tmdb_id: i64, reservation: AddReservation, written: WriteOutcome) {
        let settled = self
            .store
            .write(move |tx| {
                let key = search_key(tmdb_id);
                match written {
                    WriteOutcome::Rejected => match reservation.previous {
                        Some(previous) => tx.upsert(&previous, ttl()),
                        None => tx.delete::<MovieSearchData>(&key).map(|_| ()),
                    },
                    WriteOutcome::Accepted | WriteOutcome::Uncertain => tx
                        .update::<MovieSearchData>(
                            &key,
                            |row| MovieSearchData {
                                state: if written == WriteOutcome::Accepted {
                                    SearchState::QueuedAfterAdd
                                } else {
                                    SearchState::Uncertain
                                },
                                ..row
                            },
                            Default::default(),
                        )
                        .map(|_| ()),
                }
            })
            .await;
        if let Err(error) = settled {
            store_failure(error);
        }
    }
}

impl MovieAddLedger for RadarrMovies {
    fn reserve(&self, tmdb_id: i64) -> BoxFuture<'_, Option<AddReservation>> {
        Box::pin(async move {
            match self.reserve_add(tmdb_id).await {
                Ok(previous) => Some(AddReservation { previous }),
                Err(error) => {
                    store_failure(error);
                    None
                }
            }
        })
    }

    fn settle(
        &self,
        tmdb_id: i64,
        reservation: AddReservation,
        outcome: WriteOutcome,
    ) -> BoxFuture<'_, ()> {
        Box::pin(self.settle_add(tmdb_id, reservation, outcome))
    }
}

/// A movie add's search reservation, carrying the record it replaced.
pub struct AddReservation {
    previous: Option<MovieSearchData>,
}

/// The result of the part of a grab that runs outside the write lock.
enum Prepared {
    /// Nothing to grab; the summary says why.
    Done(Box<GrabSummary>),
    Ready(Box<PreparedGrab>),
}

struct PreparedGrab {
    movie: MovieResource,
    /// The chosen release from the fresh search, approved by Radarr.
    found: ReleaseResource,
}

enum Reserved {
    /// Reserved; the record it replaced, restored if the request is rejected.
    Fresh(Option<MovieSearchData>),
    /// A record inside the window blocks it.
    Blocked(MovieSearchVia),
    Failed,
}

fn recent(via: MovieSearchVia) -> MovieSearchOutcome {
    match via {
        MovieSearchVia::Grab => MovieSearchOutcome::RecentlyGrabbed,
        MovieSearchVia::Add | MovieSearchVia::Search => MovieSearchOutcome::RecentlySearched,
    }
}

fn failure(error: &InspectError) -> MovieResult {
    match error {
        InspectError::Http { .. } => MovieResult::Error,
        InspectError::NotConfigured(_)
        | InspectError::Unavailable(_)
        | InspectError::QueueTruncated(_) => MovieResult::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(managed: usize, unmanaged: Option<usize>) -> LibraryFiles {
        let file = |i| ManagedFile {
            id: Some(i64::try_from(i).unwrap_or_default()),
            relative_path: None,
            size_bytes: None,
            quality: None,
        };
        let extra = || UnmanagedFile {
            path: Some("/movies/x/old.mkv".to_owned()),
            size_bytes: None,
            quality: None,
            rejections: Vec::new(),
        };
        LibraryFiles::new(
            (0..managed).map(file).collect(),
            unmanaged.map(|n| (0..n).map(|_| extra()).collect()),
        )
    }

    #[test]
    fn only_one_managed_file_and_a_clean_folder_count_as_exactly_one_file() {
        assert!(library(1, Some(0)).exactly_one_file);
        assert!(!library(1, Some(1)).exactly_one_file);
        assert!(!library(2, Some(0)).exactly_one_file);
        assert!(!library(0, Some(0)).exactly_one_file);
        // A folder that could not be scanned proves nothing.
        assert!(!library(1, None).exactly_one_file);
    }

    #[test]
    fn an_upgrade_is_verified_only_when_the_file_changed_and_is_alone() {
        assert_eq!(
            upgrade_state(5, Some(5), false, None),
            UpgradeState::Pending
        );
        assert_eq!(
            upgrade_state(5, Some(5), true, None),
            UpgradeState::Downloading
        );
        assert_eq!(
            upgrade_state(5, None, false, None),
            UpgradeState::FileMissing
        );
        assert_eq!(
            upgrade_state(5, Some(6), false, None),
            UpgradeState::Imported
        );
        assert_eq!(
            upgrade_state(5, Some(6), false, Some(&library(1, Some(0)))),
            UpgradeState::Verified
        );
        assert_eq!(
            upgrade_state(5, Some(6), false, Some(&library(1, Some(1)))),
            UpgradeState::ExtraFiles
        );
        assert_eq!(
            upgrade_state(5, Some(6), false, Some(&library(2, Some(0)))),
            UpgradeState::ExtraFiles
        );
        assert_eq!(
            upgrade_state(5, Some(6), false, Some(&library(1, None))),
            UpgradeState::Imported
        );
    }

    #[test]
    fn a_library_wide_or_naming_search_covers_the_movie_while_active() {
        let command = |name: &str, status: &str, ids: Option<Vec<i64>>| CommandResource {
            id: Some(1),
            name: Some(name.to_owned()),
            status: Some(status.to_owned()),
            body: Some(CommandBody { movie_ids: ids }),
        };
        assert!(command("MoviesSearch", "queued", Some(vec![3, 4])).searches(4));
        assert!(!command("MoviesSearch", "started", Some(vec![3])).searches(4));
        assert!(!command("MoviesSearch", "completed", Some(vec![4])).searches(4));
        assert!(command("MissingMoviesSearch", "started", None).searches(4));
        assert!(command("CutoffUnmetMoviesSearch", "queued", None).searches(4));
        assert!(!command("RssSync", "started", None).searches(4));
    }
}
