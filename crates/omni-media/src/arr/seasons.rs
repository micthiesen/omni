//! Season-level Sonarr management: inspect a series, add it with selected
//! seasons, change which seasons are monitored, and search selected seasons.
//!
//! Monitoring and searching stay separate. Monitoring only lets Sonarr grab
//! new releases; a search grabs what is already missing. Adding a series never
//! searches unless asked, and then only through Sonarr's one post-add search,
//! which covers monitored episodes (the selected seasons) once its first
//! refresh finishes. Every search Omni starts or asks for is reserved durably
//! per season, so a later call within [`SEARCH_WINDOW_MS`] reports
//! `recently_searched` instead of searching twice.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;

use omni_core::spawn::must_complete;
use omni_http::{Method, SideEffectMode};
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps as _, EntityWrite as _, UpsertOpts};
use omni_store::{DocOps as _, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::task::TaskTracker;

use super::inspect::{ArrService, InspectError, fetch_profiles};
use super::sonarr::{AddSearchLedger, add_sonarr_series_with};
use super::{ArrConfig, ArrHttp, HttpResult, WriteOutcome, put_opt};
use crate::types::WatchlistAddOutcome;

const LOG: &str = "Sonarr";
const HOUR_MS: i64 = 60 * 60 * 1000;
/// A season searched (or queued for the post-add search) within this window
/// is not searched again unless the caller forces it.
pub const SEARCH_WINDOW_MS: i64 = 24 * HOUR_MS;
const SEARCH_RECORD_TTL_MS: i64 = 7 * 24 * HOUR_MS;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesResource {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    title_slug: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    tvdb_id: Option<i64>,
    #[serde(default)]
    tmdb_id: Option<i64>,
    #[serde(default)]
    imdb_id: Option<String>,
    #[serde(default)]
    quality_profile_id: Option<i64>,
    #[serde(default)]
    monitored: Option<bool>,
    /// Sonarr v4 only: `all` monitors seasons that appear later, `none` does not.
    #[serde(default)]
    monitor_new_items: Option<String>,
    #[serde(default)]
    seasons: Vec<SeasonResource>,
    /// Present until Sonarr's first refresh after the add has run.
    #[serde(default)]
    add_options: Option<AddOptions>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeasonResource {
    season_number: i64,
    #[serde(default)]
    monitored: Option<bool>,
    #[serde(default)]
    statistics: Option<SeasonStatistics>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeasonStatistics {
    /// Monitored episodes that have aired, plus episodes with files.
    #[serde(default)]
    episode_count: Option<i64>,
    #[serde(default)]
    episode_file_count: Option<i64>,
    #[serde(default)]
    total_episode_count: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddOptions {
    #[serde(default)]
    search_for_missing_episodes: Option<bool>,
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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandBody {
    #[serde(default)]
    series_id: Option<i64>,
    #[serde(default)]
    series_ids: Option<Vec<i64>>,
    #[serde(default)]
    season_number: Option<i64>,
}

impl CommandResource {
    fn is_active(&self) -> bool {
        matches!(self.status.as_deref(), Some("queued" | "started"))
    }

    /// The season this active search covers for `series_id`: `Some(None)`
    /// for a whole-series search, `None` when it is not a search of the series.
    fn searched_season(&self, series_id: i64) -> Option<Option<i64>> {
        if !self.is_active() {
            return None;
        }
        let body = self.body.as_ref()?;
        if self.name.as_deref() == Some("MissingEpisodeSearch") && body.series_id.is_none() {
            // "Search all missing" covers every monitored series.
            return Some(None);
        }
        let covers_series = body.series_id == Some(series_id)
            || body
                .series_ids
                .as_ref()
                .is_some_and(|ids| ids.contains(&series_id));
        if !covers_series {
            return None;
        }
        match self.name.as_deref()? {
            "SeasonSearch" => Some(body.season_number),
            "SeriesSearch" | "MissingEpisodeSearch" => Some(None),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueItem {
    #[serde(default)]
    season_number: Option<i64>,
    #[serde(default)]
    episode: Option<QueueEpisode>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueEpisode {
    #[serde(default)]
    season_number: Option<i64>,
}

/// Durable per-season search reservation, keyed by `<tvdbId>:<seasonNumber>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeasonSearchData {
    pub tvdb_id: i64,
    pub season_number: i64,
    pub state: SearchState,
    pub via: SearchVia,
    pub requested_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for SeasonSearchData {
    const NAME: &'static str = "media-season-search";
    type Key = String;
    fn key(&self) -> String {
        search_key(self.tvdb_id, self.season_number)
    }
}

fn search_key(tvdb_id: i64, season_number: i64) -> String {
    format!("{tvdb_id}:{season_number}")
}

/// `reserved` and `uncertain` mean the request may have reached Sonarr; they
/// block a repeat exactly like a confirmed search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchState {
    Reserved,
    Queued,
    QueuedAfterAdd,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchVia {
    Add,
    Search,
}

/// The last search Omni started or queued for a season.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastSearch {
    pub requested_at: i64,
    pub via: SearchVia,
    pub state: SearchState,
}

impl From<&SeasonSearchData> for LastSearch {
    fn from(row: &SeasonSearchData) -> Self {
        Self {
            requested_at: row.requested_at,
            via: row.via,
            state: row.state,
        }
    }
}

/// One season as Sonarr reports it after any change.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeasonStatus {
    pub season_number: i64,
    pub monitored: bool,
    pub episode_count: Option<i64>,
    pub episode_file_count: Option<i64>,
    pub total_episode_count: Option<i64>,
    pub missing_episodes: Option<i64>,
    pub queued_episodes: i64,
    pub last_search: Option<LastSearch>,
}

/// An active Sonarr search touching the series.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSearch {
    pub name: String,
    pub status: String,
    pub season_number: Option<i64>,
}

/// A series as Sonarr tracks it (or, untracked, as Sonarr's lookup sees it).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesStatus {
    pub tracked: bool,
    pub sonarr_id: Option<i64>,
    pub title: String,
    pub year: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: i64,
    pub title_slug: Option<String>,
    pub quality_profile_id: Option<i64>,
    pub monitored: bool,
    pub monitor_new_seasons: Option<bool>,
    pub initial_search_pending: bool,
    pub initial_refresh_pending: bool,
    pub seasons: Vec<SeasonStatus>,
    pub active_searches: Vec<ActiveSearch>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesResult {
    Tracked,
    NotTracked,
    Added,
    Updated,
    Unchanged,
    Searched,
    NothingSearched,
    NotFound,
    SeasonNotFound,
    /// A profile change that was only planned (`dryRun`).
    Preview,
    ProfileNotFound,
    Unavailable,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOutcome {
    /// Sonarr accepted a season search now.
    Queued,
    /// Sonarr runs its one search for the new series after the first refresh.
    QueuedAfterAdd,
    /// The new series' post-add search has not run yet.
    InitialSearchPending,
    /// Sonarr has not finished the first refresh of a series added without a
    /// search, so its episodes are not ready to search yet.
    RefreshPending,
    /// A matching Sonarr search is queued or running.
    AlreadyRunning,
    /// Omni searched or queued this season within the search window.
    RecentlySearched,
    NotMonitored,
    NothingMissing,
    /// The request may have reached Sonarr; it is not retried.
    Uncertain,
    /// Sonarr rejected the search; nothing was started.
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeasonSearchResult {
    pub season_number: i64,
    pub outcome: SearchOutcome,
    pub command_id: Option<i64>,
}

/// What a season operation did and the series state Sonarr reports after it.
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesOutcome {
    pub result: SeriesResult,
    pub series: Option<SeriesStatus>,
    pub changed_seasons: Vec<i64>,
    pub unknown_seasons: Vec<i64>,
    pub search: Vec<SeasonSearchResult>,
}

impl SeriesOutcome {
    fn of(result: SeriesResult) -> Self {
        Self {
            result,
            series: None,
            changed_seasons: Vec::new(),
            unknown_seasons: Vec::new(),
            search: Vec::new(),
        }
    }

    fn with_series(result: SeriesResult, series: SeriesStatus) -> Self {
        Self {
            series: Some(series),
            ..Self::of(result)
        }
    }
}

/// Why an operation stopped early; it becomes the outcome's result.
#[derive(Clone, Copy, Debug)]
struct Failure(SeriesResult);

impl From<Failure> for SeriesOutcome {
    fn from(failure: Failure) -> Self {
        Self::of(failure.0)
    }
}

fn http_failure<T>(result: &HttpResult<T>) -> Failure {
    Failure(match result {
        HttpResult::HttpError { .. } => SeriesResult::Error,
        _ => SeriesResult::Unavailable,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeasonAddRequest {
    pub tmdb_id: i64,
    pub seasons: BTreeSet<i64>,
    pub search: bool,
    pub monitor_new_seasons: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeasonSetRequest {
    pub tmdb_id: i64,
    pub monitor: BTreeSet<i64>,
    pub unmonitor: BTreeSet<i64>,
    pub monitor_new_seasons: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesProfileRequest {
    pub tmdb_id: i64,
    pub quality_profile_id: i64,
    pub dry_run: bool,
}

/// A series profile change: the result, the profile before it, and the
/// series as Sonarr reports it afterwards (as it is, for a preview).
#[derive(Clone, Debug, PartialEq)]
pub struct SeriesProfileOutcome {
    pub result: SeriesResult,
    pub previous_quality_profile_id: Option<i64>,
    pub quality_profile: Option<(i64, String)>,
    pub series: Option<SeriesStatus>,
}

impl SeriesProfileOutcome {
    fn of(result: SeriesResult) -> Self {
        Self {
            result,
            previous_quality_profile_id: None,
            quality_profile: None,
            series: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeasonSearchRequest {
    pub tmdb_id: i64,
    pub seasons: BTreeSet<i64>,
    pub force: bool,
}

/// A tracked series: Sonarr's raw resource (written back on update) and its parse.
struct Tracked {
    id: i64,
    tvdb_id: i64,
    raw: Value,
    series: SeriesResource,
}

enum Resolved {
    Tracked(Box<Tracked>),
    NotTracked(Box<SeriesResource>),
}

type Step<T> = Result<T, Failure>;

fn step<T>(result: HttpResult<T>) -> Step<T> {
    match result {
        HttpResult::Ok(value) => Ok(value),
        failure => Err(http_failure(&failure)),
    }
}

/// Season operations over the configured Sonarr. Mutations are serialized
/// and run to completion even when the calling request is dropped.
#[derive(Clone)]
pub struct SonarrSeasons {
    http: ArrHttp,
    config: ArrConfig,
    store: Store,
    tracker: TaskTracker,
    writes: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl SonarrSeasons {
    pub fn new(http: ArrHttp, config: ArrConfig, store: Store, tracker: TaskTracker) -> Self {
        Self {
            http,
            config,
            store,
            tracker,
            writes: std::sync::Arc::default(),
        }
    }

    /// The series, its seasons, download progress and search state.
    pub async fn status(&self, tmdb_id: i64) -> SeriesOutcome {
        self.status_inner(tmdb_id)
            .await
            .unwrap_or_else(SeriesOutcome::from)
    }

    pub async fn add(&self, request: SeasonAddRequest) -> SeriesOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.add_inner(request)
                .await
                .unwrap_or_else(SeriesOutcome::from)
        })
        .await
    }

    pub async fn set(&self, request: SeasonSetRequest) -> SeriesOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.set_inner(request)
                .await
                .unwrap_or_else(SeriesOutcome::from)
        })
        .await
    }

    pub async fn search(&self, request: SeasonSearchRequest) -> SeriesOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.search_inner(request)
                .await
                .unwrap_or_else(SeriesOutcome::from)
        })
        .await
    }

    /// Changes the series' quality profile, leaving monitoring alone. Never
    /// searches: Sonarr grabs upgrades for monitored episodes as releases appear.
    pub async fn set_profile(&self, request: SeriesProfileRequest) -> SeriesProfileOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            this.set_profile_inner(request)
                .await
                .unwrap_or_else(|failure| SeriesProfileOutcome::of(failure.0))
        })
        .await
    }

    /// A whole-series add (`media_watchlist_add` or a recommendation) with
    /// its post-add search recorded for every season, serialized with the
    /// season tools.
    pub async fn add_whole_series(&self, tmdb_id: i64) -> WatchlistAddOutcome {
        let this = self.clone();
        must_complete(&self.tracker, async move {
            let _writes = this.writes.lock().await;
            add_sonarr_series_with(&this.http, &this.config, tmdb_id, Some(&this)).await
        })
        .await
    }

    async fn status_inner(&self, tmdb_id: i64) -> Step<SeriesOutcome> {
        let Some(connection) = self.config.connection() else {
            return Ok(SeriesOutcome::of(SeriesResult::Unavailable));
        };
        Ok(match self.resolve(connection, tmdb_id).await? {
            Resolved::Tracked(tracked) => SeriesOutcome::with_series(
                SeriesResult::Tracked,
                self.tracked_status(connection, &tracked).await?,
            ),
            Resolved::NotTracked(lookup) => {
                SeriesOutcome::with_series(SeriesResult::NotTracked, untracked_status(&lookup))
            }
        })
    }

    async fn add_inner(&self, request: SeasonAddRequest) -> Step<SeriesOutcome> {
        let Some(connection) = self.config.connection() else {
            return Ok(SeriesOutcome::of(SeriesResult::Unavailable));
        };
        let lookup = match self.resolve(connection, request.tmdb_id).await? {
            Resolved::Tracked(tracked) => {
                let id = tracked.id;
                let mut outcome = self
                    .apply_monitoring(
                        connection,
                        *tracked,
                        &request.seasons,
                        &BTreeSet::new(),
                        request.monitor_new_seasons,
                    )
                    .await?;
                if request.search && outcome.result != SeriesResult::Error {
                    let tracked = self.tracked(connection, id).await?;
                    outcome.search = self
                        .search_seasons(connection, &tracked, &request.seasons, false)
                        .await?;
                    outcome.series = Some(self.tracked_status(connection, &tracked).await?);
                }
                return Ok(outcome);
            }
            Resolved::NotTracked(lookup) => lookup,
        };
        let unknown = unknown_seasons(&lookup.seasons, &request.seasons);
        if !unknown.is_empty() {
            return Ok(SeriesOutcome {
                unknown_seasons: unknown,
                ..SeriesOutcome::with_series(
                    SeriesResult::SeasonNotFound,
                    untracked_status(&lookup),
                )
            });
        }
        let Some((root_folder_path, quality_profile_id)) = self.config.acquisition() else {
            return Ok(SeriesOutcome::of(SeriesResult::Unavailable));
        };
        let (Some(title), Some(tvdb_id)) = (lookup.title.clone(), lookup.tvdb_id) else {
            return Ok(SeriesOutcome::of(SeriesResult::NotFound));
        };

        let mut body = serde_json::Map::new();
        body.insert("title".to_owned(), title.into());
        put_opt(&mut body, "titleSlug", lookup.title_slug.clone());
        put_opt(&mut body, "year", lookup.year);
        body.insert("tvdbId".to_owned(), tvdb_id.into());
        put_opt(&mut body, "tmdbId", lookup.tmdb_id);
        put_opt(&mut body, "imdbId", lookup.imdb_id.clone());
        body.insert("qualityProfileId".to_owned(), quality_profile_id.into());
        body.insert("rootFolderPath".to_owned(), root_folder_path.into());
        body.insert("monitored".to_owned(), true.into());
        body.insert("seasonFolder".to_owned(), true.into());
        let new_items = if request.monitor_new_seasons.unwrap_or(false) {
            "all"
        } else {
            "none"
        };
        body.insert("monitorNewItems".to_owned(), new_items.into());
        // Without `addOptions.monitor`, Sonarr keeps these season flags and
        // unmonitors every episode of an unmonitored season.
        let seasons: Vec<Value> = lookup
            .seasons
            .iter()
            .map(|season| {
                serde_json::json!({
                    "seasonNumber": season.season_number,
                    "monitored": request.seasons.contains(&season.season_number),
                })
            })
            .collect();
        body.insert("seasons".to_owned(), seasons.into());
        body.insert(
            "addOptions".to_owned(),
            serde_json::json!({ "searchForMissingEpisodes": request.search }),
        );
        let body = Value::Object(body);

        let search = |outcome| {
            if request.search {
                request
                    .seasons
                    .iter()
                    .map(|&season_number| SeasonSearchResult {
                        season_number,
                        outcome,
                        command_id: None,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, "series", body);
            return Ok(SeriesOutcome {
                changed_seasons: request.seasons.iter().copied().collect(),
                search: search(SearchOutcome::QueuedAfterAdd),
                ..SeriesOutcome::of(SeriesResult::Added)
            });
        }

        let reserved: Vec<i64> = request.seasons.iter().copied().collect();
        if request.search {
            self.reserve_add_searches(tvdb_id, &reserved, SearchState::Reserved)
                .await
                .map_err(store_failure)?;
        }
        let created = self
            .http
            .request_json::<SeriesResource>(connection, "series", Some(body))
            .await;
        let written = WriteOutcome::of(&created);
        if request.search {
            self.settle_add_searches(tvdb_id, &reserved, written).await;
        }
        let created = match created {
            HttpResult::Ok(created) => created,
            failure => {
                let mut outcome = SeriesOutcome::from(http_failure(&failure));
                outcome.search = search(match written {
                    WriteOutcome::Rejected => SearchOutcome::Failed,
                    _ => SearchOutcome::Uncertain,
                });
                return Ok(outcome);
            }
        };
        tracing::info!(
            target: LOG,
            "Added series tvdb:{tvdb_id} with seasons {:?} (search: {})",
            request.seasons,
            request.search
        );

        // The add happened: a failed read-back leaves `series` null, never an error.
        let series = match created.id {
            Some(id) => match self.tracked(connection, id).await {
                Ok(tracked) => self.tracked_status(connection, &tracked).await.ok(),
                Err(_) => None,
            },
            None => None,
        };
        Ok(SeriesOutcome {
            series,
            changed_seasons: reserved,
            search: search(SearchOutcome::QueuedAfterAdd),
            ..SeriesOutcome::of(SeriesResult::Added)
        })
    }

    async fn set_inner(&self, request: SeasonSetRequest) -> Step<SeriesOutcome> {
        let Some(connection) = self.config.connection() else {
            return Ok(SeriesOutcome::of(SeriesResult::Unavailable));
        };
        let tracked = match self.resolve(connection, request.tmdb_id).await? {
            Resolved::Tracked(tracked) => *tracked,
            Resolved::NotTracked(lookup) => {
                return Ok(SeriesOutcome::with_series(
                    SeriesResult::NotTracked,
                    untracked_status(&lookup),
                ));
            }
        };
        let requested: BTreeSet<i64> = request.monitor.union(&request.unmonitor).copied().collect();
        let unknown = unknown_seasons(&tracked.series.seasons, &requested);
        if !unknown.is_empty() {
            return Ok(SeriesOutcome {
                unknown_seasons: unknown,
                ..SeriesOutcome::with_series(
                    SeriesResult::SeasonNotFound,
                    self.tracked_status(connection, &tracked).await?,
                )
            });
        }
        self.apply_monitoring(
            connection,
            tracked,
            &request.monitor,
            &request.unmonitor,
            request.monitor_new_seasons,
        )
        .await
    }

    async fn search_inner(&self, request: SeasonSearchRequest) -> Step<SeriesOutcome> {
        let Some(connection) = self.config.connection() else {
            return Ok(SeriesOutcome::of(SeriesResult::Unavailable));
        };
        let tracked = match self.resolve(connection, request.tmdb_id).await? {
            Resolved::Tracked(tracked) => *tracked,
            Resolved::NotTracked(lookup) => {
                return Ok(SeriesOutcome::with_series(
                    SeriesResult::NotTracked,
                    untracked_status(&lookup),
                ));
            }
        };
        let unknown = unknown_seasons(&tracked.series.seasons, &request.seasons);
        if !unknown.is_empty() {
            return Ok(SeriesOutcome {
                unknown_seasons: unknown,
                ..SeriesOutcome::with_series(
                    SeriesResult::SeasonNotFound,
                    self.tracked_status(connection, &tracked).await?,
                )
            });
        }
        let search = self
            .search_seasons(connection, &tracked, &request.seasons, request.force)
            .await?;
        let started = search
            .iter()
            .any(|s| matches!(s.outcome, SearchOutcome::Queued | SearchOutcome::Uncertain));
        Ok(SeriesOutcome {
            search,
            // Searches already ran: a failed read-back leaves `series` null.
            series: self.tracked_status(connection, &tracked).await.ok(),
            ..SeriesOutcome::of(if started {
                SeriesResult::Searched
            } else {
                SeriesResult::NothingSearched
            })
        })
    }

    async fn set_profile_inner(&self, request: SeriesProfileRequest) -> Step<SeriesProfileOutcome> {
        let Some(connection) = self.config.connection() else {
            return Ok(SeriesProfileOutcome::of(SeriesResult::Unavailable));
        };
        let tracked = match self.resolve(connection, request.tmdb_id).await? {
            Resolved::Tracked(tracked) => *tracked,
            Resolved::NotTracked(lookup) => {
                return Ok(SeriesProfileOutcome {
                    series: Some(untracked_status(&lookup)),
                    ..SeriesProfileOutcome::of(SeriesResult::NotTracked)
                });
            }
        };
        let profiles = fetch_profiles(&self.http, ArrService::Sonarr, connection)
            .await
            .map_err(|error| {
                Failure(match error {
                    InspectError::Http { .. } => SeriesResult::Error,
                    _ => SeriesResult::Unavailable,
                })
            })?;
        let Some(profile) = profiles
            .iter()
            .find(|p| p.id() == request.quality_profile_id)
        else {
            return Ok(SeriesProfileOutcome::of(SeriesResult::ProfileNotFound));
        };
        let before = tracked.series.quality_profile_id;
        let outcome = |result, series| SeriesProfileOutcome {
            result,
            previous_quality_profile_id: before,
            quality_profile: Some((profile.id(), profile.name())),
            series,
        };
        if before == Some(request.quality_profile_id) || request.dry_run {
            let result = if request.dry_run {
                SeriesResult::Preview
            } else {
                SeriesResult::Unchanged
            };
            return Ok(outcome(
                result,
                Some(self.tracked_status(connection, &tracked).await?),
            ));
        }
        let mut body = tracked.raw.clone();
        body["qualityProfileId"] = request.quality_profile_id.into();
        let path = format!("series/{}", tracked.id);
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, &path, body);
            return Ok(outcome(SeriesResult::Updated, None));
        }
        let sent = self
            .http
            .send_json::<Value>(connection, Method::PUT, &path, Some(body))
            .await;
        if WriteOutcome::of(&sent) == WriteOutcome::Rejected {
            return Ok(outcome(SeriesResult::Error, None));
        }
        // Accepted or uncertain alike, only Sonarr's own state settles it.
        let written = self.tracked(connection, tracked.id).await?;
        let verified = written.series.quality_profile_id == Some(request.quality_profile_id);
        tracing::info!(
            target: LOG,
            "Set quality profile {} on series tvdb:{} (verified: {verified})",
            request.quality_profile_id,
            tracked.tvdb_id
        );
        Ok(outcome(
            if verified {
                SeriesResult::Updated
            } else {
                SeriesResult::Error
            },
            Some(self.tracked_status(connection, &written).await?),
        ))
    }

    /// Tracked by TMDB id, else looked up and matched by TVDB id.
    async fn resolve(&self, connection: (&str, &str), tmdb_id: i64) -> Step<Resolved> {
        let all = step(
            self.http
                .request_json::<Vec<SeriesResource>>(connection, "series", None)
                .await,
        )?;
        let tracked_id = |matches: &dyn Fn(&SeriesResource) -> bool| {
            all.iter().filter(|s| matches(s)).find_map(|s| s.id)
        };
        if let Some(id) = tracked_id(&|s| s.tmdb_id == Some(tmdb_id)) {
            return Ok(Resolved::Tracked(Box::new(
                self.tracked(connection, id).await?,
            )));
        }
        let lookup_path = format!(
            "series/lookup?term={}",
            omni_core::js::encode_uri_component(&format!("tmdb:{tmdb_id}"))
        );
        let lookup = step(
            self.http
                .request_json::<Vec<SeriesResource>>(connection, &lookup_path, None)
                .await,
        )?;
        let Some(found) = lookup
            .into_iter()
            .next()
            .filter(|s| s.title.as_deref().is_some_and(|t| !t.is_empty()) && s.tvdb_id.is_some())
        else {
            return Err(Failure(SeriesResult::NotFound));
        };
        if let Some(id) = tracked_id(&|s| s.tvdb_id.is_some() && s.tvdb_id == found.tvdb_id) {
            return Ok(Resolved::Tracked(Box::new(
                self.tracked(connection, id).await?,
            )));
        }
        Ok(Resolved::NotTracked(Box::new(found)))
    }

    async fn tracked(&self, connection: (&str, &str), id: i64) -> Step<Tracked> {
        let raw = step(
            self.http
                .request_json::<Value>(connection, &format!("series/{id}"), None)
                .await,
        )?;
        let series: SeriesResource =
            serde_json::from_value(raw.clone()).map_err(|_| Failure(SeriesResult::Unavailable))?;
        let tvdb_id = series.tvdb_id.ok_or(Failure(SeriesResult::Error))?;
        Ok(Tracked {
            id,
            tvdb_id,
            raw,
            series,
        })
    }

    /// Monitors `monitor` and unmonitors `unmonitor`, touching nothing else.
    /// Turning on a season of an unmonitored series also turns the series on,
    /// so every season not in `monitor` is unmonitored in the same write:
    /// they were not effectively monitored before and must not start now.
    async fn apply_monitoring(
        &self,
        connection: (&str, &str),
        tracked: Tracked,
        monitor: &BTreeSet<i64>,
        unmonitor: &BTreeSet<i64>,
        monitor_new_seasons: Option<bool>,
    ) -> Step<SeriesOutcome> {
        let plan = MonitoringPlan::new(&tracked.series, monitor, unmonitor, monitor_new_seasons);
        if plan.is_noop() {
            return Ok(SeriesOutcome::with_series(
                SeriesResult::Unchanged,
                self.tracked_status(connection, &tracked).await?,
            ));
        }
        let mut body = tracked.raw.clone();
        plan.apply_to(&mut body);
        let path = format!("series/{}", tracked.id);
        if self.http.mode() == SideEffectMode::Record {
            self.http.record_write(connection.0, &path, body);
            return Ok(SeriesOutcome {
                changed_seasons: plan.changed_seasons(),
                ..SeriesOutcome::of(SeriesResult::Updated)
            });
        }
        step(
            self.http
                .send_json::<Value>(connection, Method::PUT, &path, Some(body))
                .await,
        )?;
        tracing::info!(
            target: LOG,
            "Updated season monitoring for series tvdb:{}: {:?}",
            tracked.tvdb_id,
            plan.seasons
        );
        let written = self.tracked(connection, tracked.id).await?;
        let verified = plan.matches(&written.series);
        Ok(SeriesOutcome {
            changed_seasons: plan.changed_seasons(),
            ..SeriesOutcome::with_series(
                if verified {
                    SeriesResult::Updated
                } else {
                    SeriesResult::Error
                },
                self.tracked_status(connection, &written).await?,
            )
        })
    }

    async fn search_seasons(
        &self,
        connection: (&str, &str),
        tracked: &Tracked,
        seasons: &BTreeSet<i64>,
        force: bool,
    ) -> Step<Vec<SeasonSearchResult>> {
        let commands = step(
            self.http
                .request_json::<Vec<CommandResource>>(connection, "command", None)
                .await,
        )?;
        let running: Vec<Option<i64>> = commands
            .iter()
            .filter_map(|c| c.searched_season(tracked.id))
            .collect();
        let series = &tracked.series;
        let (refresh_pending, search_pending) = (
            initial_refresh_pending(series),
            initial_search_pending(series),
        );
        let mut results = Vec::new();
        for &season_number in seasons {
            let season = series
                .seasons
                .iter()
                .find(|s| s.season_number == season_number);
            let effective =
                series.monitored == Some(true) && season.is_some_and(|s| s.monitored == Some(true));
            let missing = season.and_then(missing_episodes);
            let skip = if !effective {
                Some(SearchOutcome::NotMonitored)
            } else if search_pending {
                Some(SearchOutcome::InitialSearchPending)
            } else if refresh_pending {
                Some(SearchOutcome::RefreshPending)
            } else if running
                .iter()
                .any(|s| s.is_none() || *s == Some(season_number))
            {
                Some(SearchOutcome::AlreadyRunning)
            } else if missing == Some(0) {
                Some(SearchOutcome::NothingMissing)
            } else {
                None
            };
            let (outcome, command_id) = match skip {
                Some(outcome) => (outcome, None),
                None => {
                    self.search_season(connection, tracked, season_number, force)
                        .await
                }
            };
            results.push(SeasonSearchResult {
                season_number,
                outcome,
                command_id,
            });
        }
        Ok(results)
    }

    /// Reserve → command → record. A reservation inside the window blocks
    /// the search unless forced; an uncertain request keeps its reservation
    /// and a rejected one restores the record it replaced. Never fails: the
    /// outcome says what happened.
    async fn search_season(
        &self,
        connection: (&str, &str),
        tracked: &Tracked,
        season_number: i64,
        force: bool,
    ) -> (SearchOutcome, Option<i64>) {
        let (tvdb_id, record) = (tracked.tvdb_id, self.http.mode() == SideEffectMode::Record);
        let reserved = self
            .store
            .write(move |tx| {
                let key = search_key(tvdb_id, season_number);
                let now = tx.now_ms();
                let previous = tx.get::<SeasonSearchData>(&key)?;
                if !force
                    && previous
                        .as_ref()
                        .is_some_and(|row| now - row.requested_at < SEARCH_WINDOW_MS)
                {
                    return Ok::<_, StoreError>(None);
                }
                if !record {
                    tx.upsert(
                        &SeasonSearchData {
                            tvdb_id,
                            season_number,
                            state: SearchState::Reserved,
                            via: SearchVia::Search,
                            requested_at: now,
                            command_id: None,
                            extra: Extra::default(),
                        },
                        ttl(),
                    )?;
                }
                Ok(Some(previous))
            })
            .await;
        let previous = match reserved {
            Ok(Some(previous)) => previous,
            Ok(None) => return (SearchOutcome::RecentlySearched, None),
            Err(error) => {
                store_failure(error);
                return (SearchOutcome::Failed, None);
            }
        };
        let body = serde_json::json!({
            "name": "SeasonSearch",
            "seriesId": tracked.id,
            "seasonNumber": season_number,
        });
        if record {
            self.http.record_write(connection.0, "command", body);
            return (SearchOutcome::Queued, None);
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
        let (outcome, state) = match written {
            WriteOutcome::Accepted => (SearchOutcome::Queued, SearchState::Queued),
            WriteOutcome::Uncertain => (SearchOutcome::Uncertain, SearchState::Uncertain),
            WriteOutcome::Rejected => {
                tracing::warn!(
                    target: LOG,
                    "Sonarr rejected a search of tvdb:{tvdb_id} season {season_number}"
                );
                (SearchOutcome::Failed, SearchState::Reserved)
            }
        };
        let settled = self
            .store
            .write(move |tx| {
                let key = search_key(tvdb_id, season_number);
                if written == WriteOutcome::Rejected {
                    return match previous {
                        Some(previous) => tx.upsert(&previous, ttl()),
                        None => tx.delete::<SeasonSearchData>(&key).map(|_| ()),
                    };
                }
                tx.update::<SeasonSearchData>(
                    &key,
                    |row| SeasonSearchData {
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
        if outcome == SearchOutcome::Queued {
            tracing::info!(
                target: LOG,
                "Started a search of series tvdb:{tvdb_id} season {season_number}"
            );
        }
        (outcome, command_id)
    }

    async fn reserve_add_searches(
        &self,
        tvdb_id: i64,
        seasons: &[i64],
        state: SearchState,
    ) -> Result<(), StoreError> {
        let seasons = seasons.to_vec();
        self.store
            .write(move |tx| {
                let now = tx.now_ms();
                for season_number in seasons {
                    tx.upsert(
                        &SeasonSearchData {
                            tvdb_id,
                            season_number,
                            state,
                            via: SearchVia::Add,
                            requested_at: now,
                            command_id: None,
                            extra: Extra::default(),
                        },
                        ttl(),
                    )?;
                }
                Ok(())
            })
            .await
    }

    /// A rejected add releases the reservations; otherwise they stay, so a
    /// failure here only leaves them blocking as `reserved`.
    async fn settle_add_searches(&self, tvdb_id: i64, seasons: &[i64], written: WriteOutcome) {
        let seasons = seasons.to_vec();
        let state = match written {
            WriteOutcome::Accepted => Some(SearchState::QueuedAfterAdd),
            WriteOutcome::Uncertain => Some(SearchState::Uncertain),
            WriteOutcome::Rejected => None,
        };
        let settled = self
            .store
            .write(move |tx| {
                for season_number in seasons {
                    let key = search_key(tvdb_id, season_number);
                    match state {
                        Some(state) => {
                            tx.update::<SeasonSearchData>(
                                &key,
                                |row| SeasonSearchData { state, ..row },
                                Default::default(),
                            )?;
                        }
                        None => {
                            tx.delete::<SeasonSearchData>(&key)?;
                        }
                    }
                }
                Ok::<_, StoreError>(())
            })
            .await;
        if let Err(error) = settled {
            tracing::warn!(target: LOG, "Season search record failed: {error}");
        }
    }

    async fn tracked_status(
        &self,
        connection: (&str, &str),
        tracked: &Tracked,
    ) -> Step<SeriesStatus> {
        let queue_path = format!("queue/details?seriesId={}&includeEpisode=true", tracked.id);
        let (commands, queue) = futures::join!(
            self.http
                .request_json::<Vec<CommandResource>>(connection, "command", None),
            self.http
                .request_json::<Vec<QueueItem>>(connection, &queue_path, None)
        );
        let (commands, queue) = (step(commands)?, step(queue)?);
        let mut queued: BTreeMap<i64, i64> = BTreeMap::new();
        for item in &queue {
            if let Some(season) = item
                .episode
                .as_ref()
                .and_then(|e| e.season_number)
                .or(item.season_number)
            {
                *queued.entry(season).or_default() += 1;
            }
        }
        let tvdb_id = tracked.tvdb_id;
        let numbers: Vec<i64> = tracked
            .series
            .seasons
            .iter()
            .map(|s| s.season_number)
            .collect();
        let searches = self
            .store
            .read(move |docs| {
                let mut rows = BTreeMap::new();
                for season_number in numbers {
                    if let Some(row) =
                        docs.get::<SeasonSearchData>(&search_key(tvdb_id, season_number))?
                    {
                        rows.insert(season_number, row);
                    }
                }
                Ok(rows)
            })
            .await
            .map_err(store_failure)?;
        let series = &tracked.series;
        Ok(SeriesStatus {
            tracked: true,
            sonarr_id: Some(tracked.id),
            title: series.title.clone().unwrap_or_default(),
            year: series.year,
            tmdb_id: series.tmdb_id,
            tvdb_id,
            title_slug: series.title_slug.clone(),
            quality_profile_id: series.quality_profile_id,
            monitored: series.monitored == Some(true),
            monitor_new_seasons: monitor_new_seasons(series),
            initial_search_pending: initial_search_pending(series),
            initial_refresh_pending: initial_refresh_pending(series),
            seasons: series
                .seasons
                .iter()
                .map(|season| SeasonStatus {
                    queued_episodes: queued.get(&season.season_number).copied().unwrap_or(0),
                    last_search: searches.get(&season.season_number).map(LastSearch::from),
                    ..season_status(season)
                })
                .collect(),
            active_searches: commands
                .iter()
                .filter_map(|c| {
                    c.searched_season(tracked.id)
                        .map(|season_number| ActiveSearch {
                            name: c.name.clone().unwrap_or_default(),
                            status: c.status.clone().unwrap_or_default(),
                            season_number,
                        })
                })
                .collect(),
        })
    }
}

impl AddSearchLedger for SonarrSeasons {
    fn reserve<'a>(&'a self, tvdb_id: i64, seasons: &'a [i64]) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match self
                .reserve_add_searches(tvdb_id, seasons, SearchState::Reserved)
                .await
            {
                Ok(()) => true,
                Err(error) => {
                    store_failure(error);
                    false
                }
            }
        })
    }

    fn settle<'a>(
        &'a self,
        tvdb_id: i64,
        seasons: &'a [i64],
        outcome: WriteOutcome,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.settle_add_searches(tvdb_id, seasons, outcome))
    }
}

fn ttl() -> UpsertOpts {
    UpsertOpts {
        ttl_ms: Some(SEARCH_RECORD_TTL_MS),
        ..UpsertOpts::default()
    }
}

fn store_failure(error: StoreError) -> Failure {
    tracing::warn!(target: LOG, "Season search record failed: {error}");
    Failure(SeriesResult::Error)
}

/// Sonarr keeps `addOptions` until its first refresh after the add has
/// created the episodes and applied their monitoring.
fn initial_refresh_pending(series: &SeriesResource) -> bool {
    series.add_options.is_some()
}

fn initial_search_pending(series: &SeriesResource) -> bool {
    series
        .add_options
        .as_ref()
        .and_then(|o| o.search_for_missing_episodes)
        == Some(true)
}

fn monitor_new_seasons(series: &SeriesResource) -> Option<bool> {
    match series.monitor_new_items.as_deref() {
        Some("all") => Some(true),
        Some("none") => Some(false),
        _ => None,
    }
}

fn missing_episodes(season: &SeasonResource) -> Option<i64> {
    let stats = season.statistics.as_ref()?;
    Some((stats.episode_count? - stats.episode_file_count?).max(0))
}

fn season_status(season: &SeasonResource) -> SeasonStatus {
    let stats = season.statistics.clone().unwrap_or_default();
    SeasonStatus {
        season_number: season.season_number,
        monitored: season.monitored == Some(true),
        episode_count: stats.episode_count,
        episode_file_count: stats.episode_file_count,
        total_episode_count: stats.total_episode_count,
        missing_episodes: missing_episodes(season),
        queued_episodes: 0,
        last_search: None,
    }
}

/// Sonarr's lookup view: nothing is tracked or monitored yet.
fn untracked_status(lookup: &SeriesResource) -> SeriesStatus {
    SeriesStatus {
        tracked: false,
        sonarr_id: None,
        title: lookup.title.clone().unwrap_or_default(),
        year: lookup.year,
        tmdb_id: lookup.tmdb_id,
        tvdb_id: lookup.tvdb_id.unwrap_or_default(),
        title_slug: lookup.title_slug.clone(),
        quality_profile_id: None,
        monitored: false,
        monitor_new_seasons: None,
        initial_search_pending: false,
        initial_refresh_pending: false,
        seasons: lookup
            .seasons
            .iter()
            .map(|season| SeasonStatus {
                monitored: false,
                ..season_status(season)
            })
            .collect(),
        active_searches: Vec::new(),
    }
}

fn unknown_seasons(seasons: &[SeasonResource], requested: &BTreeSet<i64>) -> Vec<i64> {
    requested
        .iter()
        .copied()
        .filter(|n| !seasons.iter().any(|s| s.season_number == *n))
        .collect()
}

/// The desired flags after a monitoring change, against Sonarr's current ones.
struct MonitoringPlan {
    series_monitored: (bool, bool),
    /// `(seasonNumber, (before, after))` for every season.
    seasons: Vec<(i64, (bool, bool))>,
    monitor_new_items: Option<(Option<bool>, bool)>,
}

impl MonitoringPlan {
    fn new(
        series: &SeriesResource,
        monitor: &BTreeSet<i64>,
        unmonitor: &BTreeSet<i64>,
        new_seasons: Option<bool>,
    ) -> Self {
        let series_before = series.monitored == Some(true);
        let turn_on = !series_before && !monitor.is_empty();
        let seasons = series
            .seasons
            .iter()
            .map(|season| {
                let n = season.season_number;
                let before = season.monitored == Some(true);
                let after = if monitor.contains(&n) {
                    true
                } else if unmonitor.contains(&n) || turn_on {
                    false
                } else {
                    before
                };
                (n, (before, after))
            })
            .collect();
        // Sonarr v3 has no `monitorNewItems`; leave it alone there.
        let monitor_new_items = new_seasons
            .filter(|_| series.monitor_new_items.is_some())
            .map(|after| (monitor_new_seasons(series), after));
        Self {
            series_monitored: (series_before, series_before || turn_on),
            seasons,
            monitor_new_items,
        }
    }

    fn is_noop(&self) -> bool {
        self.series_monitored.0 == self.series_monitored.1
            && self.seasons.iter().all(|(_, (b, a))| b == a)
            && self
                .monitor_new_items
                .is_none_or(|(before, after)| before == Some(after))
    }

    fn changed_seasons(&self) -> Vec<i64> {
        self.seasons
            .iter()
            .filter(|(_, (b, a))| b != a)
            .map(|(n, _)| *n)
            .collect()
    }

    fn apply_to(&self, raw: &mut Value) {
        raw["monitored"] = self.series_monitored.1.into();
        if let Some((_, after)) = self.monitor_new_items {
            raw["monitorNewItems"] = if after { "all" } else { "none" }.into();
        }
        if let Some(seasons) = raw.get_mut("seasons").and_then(Value::as_array_mut) {
            for season in seasons {
                let number = season.get("seasonNumber").and_then(Value::as_i64);
                if let Some((_, (_, after))) = self.seasons.iter().find(|(n, _)| Some(*n) == number)
                {
                    season["monitored"] = (*after).into();
                }
            }
        }
    }

    fn matches(&self, written: &SeriesResource) -> bool {
        written.monitored == Some(self.series_monitored.1)
            && self.seasons.iter().all(|(n, (_, after))| {
                written
                    .seasons
                    .iter()
                    .any(|s| s.season_number == *n && s.monitored == Some(*after))
            })
            && self
                .monitor_new_items
                .is_none_or(|(_, after)| monitor_new_seasons(written) == Some(after))
    }
}
