//! Media MCP tools.

pub mod defs;

use std::collections::BTreeSet;
use std::sync::Arc;

use omni_api::media::{
    MediaType, RecommendationFeedback, RecommendationStatus, TasteProfile, js_number, js_number_opt,
};
use omni_core::js::trim;
use omni_mcp_kit::{McpTool, Page, ToolError, ToolMetaError, paginate, typed_tool};
use serde::{Deserialize, Serialize};

use crate::arr::inspect::{ArrService, Download, InspectError, ProfileSummary};
use crate::arr::movies::{
    GrabRequest, MovieSearchRequest, MoviesQuery, ProfileRef, ProfileSetRequest, SearchPurpose,
};
use crate::arr::seasons::{
    SeasonAddRequest, SeasonSearchRequest, SeasonSearchResult, SeasonSetRequest, SeriesOutcome,
    SeriesProfileRequest, SeriesResult, SeriesStatus,
};
use crate::error::IntegrationError;
use crate::persistence::{
    FeedbackInput, RecommendationData, get_all_recommendations, get_recommendation,
    set_recommendation_feedback,
};
use crate::services::MediaServices;
use crate::taste::{get_all_taste_evidence, get_latest_taste_profile};
use crate::tmdb::types::{TmdbTitle, TmdbTitleDetails};
use crate::tmdb::{DiscoverOptions, tmdb_url};
use crate::types::{ExternalIds, FetchResult, InProgressItem, MediaItem, WatchedItem};
use crate::watchlist::WatchlistAddRequest;

fn default_cursor() -> usize {
    0
}
fn default_catalog_limit() -> usize {
    10
}
fn default_limit() -> usize {
    25
}
fn default_min_votes() -> i64 {
    300
}
fn default_page() -> i64 {
    1
}

fn integration(error: &IntegrationError) -> ToolError {
    ToolError::execute(error.cause_message())
}

fn store_error(error: &omni_store::StoreError) -> ToolError {
    ToolError::execute_from(error)
}

fn require_available<T>(result: FetchResult<T>) -> Result<T, ToolError> {
    match result {
        FetchResult::Ok(value) => Ok(value),
        FetchResult::Unavailable { reason } => Err(ToolError::execute(reason)),
    }
}

/// A trimmed string that must stay non-empty.
fn non_empty_trimmed(value: &str, field: &str) -> Result<String, ToolError> {
    let trimmed = trim(value);
    if trimmed.is_empty() {
        Err(ToolError::input(format!(
            "{field}: Too small: expected string to have >=1 characters"
        )))
    } else {
        Ok(trimmed.to_owned())
    }
}

#[allow(clippy::cast_precision_loss)]
fn num(value: i64) -> f64 {
    value as f64
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogTitle {
    tmdb_id: i64,
    media_type: MediaType,
    title: String,
    year: Option<i64>,
    overview: String,
    genre_ids: Vec<i64>,
    #[serde(serialize_with = "js_number")]
    vote_average: f64,
    #[serde(serialize_with = "js_number")]
    vote_count: f64,
    #[serde(serialize_with = "js_number")]
    popularity: f64,
    poster_path: Option<String>,
    original_language: Option<String>,
    tmdb_url: String,
}

fn serialize_title(title: TmdbTitle) -> CatalogTitle {
    CatalogTitle {
        tmdb_url: tmdb_url(title.media_type, title.tmdb_id),
        tmdb_id: title.tmdb_id,
        media_type: title.media_type,
        title: title.title,
        year: title.year,
        overview: title.overview,
        genre_ids: title.genre_ids,
        vote_average: title.vote_average,
        vote_count: title.vote_count,
        popularity: title.popularity,
        poster_path: title.poster_path,
        original_language: title.original_language,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolMediaItem {
    guid: String,
    title: String,
    year: Option<i64>,
    media_type: MediaType,
    external_ids: Option<ExternalIds>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_number_opt"
    )]
    progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_viewed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    view_count: Option<i64>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "js_number_opt"
    )]
    completion: Option<f64>,
}

impl ToolMediaItem {
    fn base(item: MediaItem) -> Self {
        Self {
            guid: item.guid,
            title: item.title,
            year: item.year,
            media_type: item.media_type,
            external_ids: item.external_ids,
            progress: None,
            last_viewed_at: None,
            viewed_at: None,
            view_count: None,
            completion: None,
        }
    }
    fn watched(item: WatchedItem) -> Self {
        Self {
            viewed_at: Some(item.viewed_at),
            view_count: Some(item.view_count),
            completion: item.completion,
            ..Self::base(item.item)
        }
    }
    fn in_progress(item: InProgressItem) -> Self {
        Self {
            progress: Some(item.progress),
            last_viewed_at: Some(item.last_viewed_at),
            ..Self::base(item.item)
        }
    }
}

/// Media-type and case-insensitive title filters (`toLocaleLowerCase` includes).
fn filter_items(
    items: Vec<ToolMediaItem>,
    media_type: Option<MediaType>,
    query: Option<&str>,
) -> Vec<ToolMediaItem> {
    let needle = query
        .map(trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    items
        .into_iter()
        .filter(|item| media_type.is_none_or(|t| item.media_type == t))
        .filter(|item| {
            needle
                .as_deref()
                .is_none_or(|needle| item.title.to_lowercase().contains(needle))
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolRecommendation {
    recommendation_id: String,
    canonical_id: String,
    tmdb_id: i64,
    media_type: MediaType,
    title: String,
    year: Option<i64>,
    status: RecommendationStatus,
    why_for_user: Option<String>,
    caveats: Vec<String>,
    #[serde(serialize_with = "js_number_opt")]
    confidence: Option<f64>,
    genres: Vec<String>,
    #[serde(serialize_with = "js_number_opt")]
    runtime_minutes: Option<f64>,
    #[serde(serialize_with = "js_number_opt")]
    season_count: Option<f64>,
    #[serde(serialize_with = "js_number_opt")]
    episode_count: Option<f64>,
    run_date: String,
    recommended_at: i64,
    #[serde(serialize_with = "js_number_opt")]
    notified_at: Option<f64>,
    #[serde(serialize_with = "js_number_opt")]
    resolved_at: Option<f64>,
    watchlist_result: Option<String>,
    feedback: Option<RecommendationFeedback>,
    #[serde(serialize_with = "js_number_opt")]
    feedback_at: Option<f64>,
    feedback_note: Option<String>,
}

fn serialize_recommendation(rec: &RecommendationData) -> ToolRecommendation {
    ToolRecommendation {
        recommendation_id: rec.recommendation_id.clone(),
        canonical_id: rec.canonical_id.clone(),
        tmdb_id: rec.tmdb_id,
        media_type: rec.media_type,
        title: rec.title.clone(),
        year: rec.year,
        status: rec.status(),
        why_for_user: rec.why_for_user.clone(),
        caveats: rec.caveats.clone().unwrap_or_default(),
        confidence: rec.confidence,
        genres: rec.genres.clone().unwrap_or_default(),
        runtime_minutes: rec.runtime_minutes,
        season_count: rec.season_count,
        episode_count: rec.episode_count,
        run_date: rec.run_date.clone(),
        recommended_at: rec.recommended_at,
        notified_at: rec.notified_at.map(num),
        resolved_at: rec.resolved_at.map(num),
        watchlist_result: rec.watchlist_result.map(|w| w.as_str().to_owned()),
        feedback: rec.feedback,
        feedback_at: rec.feedback_at.map(num),
        feedback_note: rec.feedback_note.clone(),
    }
}

#[derive(Serialize)]
struct RecommendationOutput {
    recommendation: ToolRecommendation,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSearchInput {
    query: String,
    media_type: MediaType,
    year: Option<i64>,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_catalog_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogGetInput {
    media_type: MediaType,
    tmdb_id: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDetails {
    genres: Vec<String>,
    #[serde(serialize_with = "js_number_opt")]
    runtime_minutes: Option<f64>,
    #[serde(serialize_with = "js_number_opt")]
    season_count: Option<f64>,
    #[serde(serialize_with = "js_number_opt")]
    episode_count: Option<f64>,
    series_status: Option<String>,
    original_language: Option<String>,
    origin_countries: Vec<String>,
    creators: Vec<String>,
    cast: Vec<String>,
    keywords: Vec<String>,
    certification: Option<String>,
}

impl From<TmdbTitleDetails> for CatalogDetails {
    fn from(d: TmdbTitleDetails) -> Self {
        Self {
            genres: d.genres,
            runtime_minutes: d.runtime_minutes,
            season_count: d.season_count,
            episode_count: d.episode_count,
            series_status: d.series_status,
            original_language: d.original_language,
            origin_countries: d.origin_countries,
            creators: d.creators,
            cast: d.cast,
            keywords: d.keywords,
            certification: d.certification,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogGetOutput {
    media_type: MediaType,
    tmdb_id: i64,
    tmdb_url: String,
    details: CatalogDetails,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum CatalogBrowseInput {
    Trending {
        #[serde(default = "default_cursor")]
        cursor: usize,
        #[serde(default = "default_catalog_limit")]
        limit: usize,
    },
    Discover {
        #[serde(rename = "mediaType")]
        media_type: MediaType,
        #[serde(rename = "withGenres")]
        with_genres: Option<Vec<i64>>,
        #[serde(rename = "withoutGenres")]
        without_genres: Option<Vec<i64>>,
        #[serde(rename = "originalLanguage")]
        original_language: Option<String>,
        #[serde(rename = "minVoteCount", default = "default_min_votes")]
        min_vote_count: i64,
        #[serde(default = "default_page")]
        page: i64,
        #[serde(default = "default_cursor")]
        cursor: usize,
        #[serde(default = "default_catalog_limit")]
        limit: usize,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LibraryView {
    Library,
    History,
    InProgress,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryListInput {
    view: LibraryView,
    media_type: Option<MediaType>,
    query: Option<String>,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchlistListInput {
    media_type: Option<MediaType>,
    query: Option<String>,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchlistAddInput {
    tmdb_id: i64,
    media_type: MediaType,
    title: String,
    year: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchlistAddOutput {
    result: &'static str,
    title_slug: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesGetInput {
    tmdb_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesAddInput {
    tmdb_id: i64,
    seasons: BTreeSet<i64>,
    search: bool,
    monitor_new_seasons: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesSeasonsSetInput {
    tmdb_id: i64,
    #[serde(default)]
    monitor: BTreeSet<i64>,
    #[serde(default)]
    unmonitor: BTreeSet<i64>,
    monitor_new_seasons: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesSearchInput {
    tmdb_id: i64,
    seasons: BTreeSet<i64>,
    #[serde(default)]
    force: bool,
}

#[derive(Serialize)]
struct SeriesGetOutput {
    result: SeriesResult,
    series: Option<SeriesStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesAddOutput {
    result: SeriesResult,
    series: Option<SeriesStatus>,
    changed_seasons: Vec<i64>,
    unknown_seasons: Vec<i64>,
    search: Vec<SeasonSearchResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesSeasonsSetOutput {
    result: SeriesResult,
    series: Option<SeriesStatus>,
    changed_seasons: Vec<i64>,
    unknown_seasons: Vec<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesSearchOutput {
    result: SeriesResult,
    series: Option<SeriesStatus>,
    unknown_seasons: Vec<i64>,
    seasons: Vec<SeasonSearchResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeriesProfileSetInput {
    tmdb_id: i64,
    quality_profile_id: i64,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeriesProfileSetOutput {
    result: SeriesResult,
    previous_quality_profile_id: Option<i64>,
    quality_profile: Option<ProfileRef>,
    series: Option<SeriesStatus>,
}

#[derive(Deserialize)]
struct ArrServiceInput {
    service: ArrService,
}

#[derive(Serialize)]
struct ArrProfilesOutput {
    service: ArrService,
    profiles: Vec<ProfileSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadsListInput {
    service: ArrService,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Serialize)]
struct DownloadsListOutput {
    service: ArrService,
    truncated: bool,
    #[serde(flatten)]
    page: Page<Download>,
}

/// The most movies one `detail` page reads in depth.
const DETAIL_LIMIT: usize = 25;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoviesGetInput {
    tmdb_ids: Option<BTreeSet<i64>>,
    query: Option<String>,
    #[serde(default)]
    cutoff_unmet: bool,
    #[serde(default)]
    missing: bool,
    #[serde(default)]
    detail: bool,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieCollectionsInput {
    query: Option<String>,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_catalog_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoviesProfileSetInput {
    tmdb_ids: BTreeSet<i64>,
    quality_profile_id: i64,
    monitored: Option<bool>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoviesSearchInput {
    tmdb_ids: BTreeSet<i64>,
    purpose: SearchPurpose,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieReleasesInput {
    tmdb_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MovieReleaseGrabInput {
    tmdb_id: i64,
    release_id: String,
    #[serde(default)]
    force: bool,
}

fn inspect_error(error: &InspectError) -> ToolError {
    ToolError::execute(error.to_string())
}

impl From<SeriesOutcome> for SeriesAddOutput {
    fn from(o: SeriesOutcome) -> Self {
        Self {
            result: o.result,
            series: o.series,
            changed_seasons: o.changed_seasons,
            unknown_seasons: o.unknown_seasons,
            search: o.search,
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FeedbackFilter {
    GoodPick,
    NotForMe,
    AlreadyWatched,
    None,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationsListInput {
    status: Option<RecommendationStatus>,
    feedback: Option<FeedbackFilter>,
    #[serde(default = "default_cursor")]
    cursor: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationGetInput {
    recommendation_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecommendationFeedbackInput {
    recommendation_id: String,
    feedback: Option<RecommendationFeedback>,
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "resource", rename_all = "lowercase")]
enum TasteReadInput {
    Profile {},
    Evidence {
        #[serde(default = "default_cursor")]
        cursor: usize,
        #[serde(default = "default_limit")]
        limit: usize,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceItem {
    evidence_id: String,
    kind: &'static str,
    canonical_id: String,
    title: String,
    media_type: MediaType,
    observed_at: i64,
    #[serde(serialize_with = "js_number_opt")]
    completion: Option<f64>,
    recommendation_id: Option<String>,
    feedback: Option<RecommendationFeedback>,
    note: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "resource", rename_all = "lowercase")]
enum TasteReadOutput {
    Profile {
        profile: Option<Box<TasteProfile>>,
    },
    Evidence {
        #[serde(flatten)]
        page: Page<EvidenceItem>,
    },
}

/// The media tools in serving order.
pub fn media_tools(services: Arc<MediaServices>) -> Result<Vec<McpTool>, ToolMetaError> {
    let s = services;
    Ok(vec![
        typed_tool(&defs::MEDIA_CATALOG_SEARCH, {
            let s = s.clone();
            move |input: CatalogSearchInput, _cx| {
                let s = s.clone();
                async move {
                    let query = non_empty_trimmed(&input.query, "query")?;
                    let titles = s
                        .catalog
                        .search_titles(&query, input.media_type, input.year)
                        .await
                        .map_err(|e| integration(&e))?;
                    Ok(paginate(
                        titles.into_iter().map(serialize_title).collect(),
                        input.cursor,
                        input.limit,
                    ))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_CATALOG_GET, {
            let s = s.clone();
            move |input: CatalogGetInput, _cx| {
                let s = s.clone();
                async move {
                    let details = s
                        .catalog
                        .title_details(input.media_type, input.tmdb_id)
                        .await
                        .map_err(|e| integration(&e))?;
                    Ok(CatalogGetOutput {
                        media_type: input.media_type,
                        tmdb_id: input.tmdb_id,
                        tmdb_url: tmdb_url(input.media_type, input.tmdb_id),
                        details: details.into(),
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_CATALOG_BROWSE, {
            let s = s.clone();
            move |input: CatalogBrowseInput, _cx| {
                let s = s.clone();
                async move {
                    let (titles, cursor, limit) = match input {
                        CatalogBrowseInput::Trending { cursor, limit } => {
                            (s.catalog.trending().await, cursor, limit)
                        }
                        CatalogBrowseInput::Discover {
                            media_type,
                            with_genres,
                            without_genres,
                            original_language,
                            min_vote_count,
                            page,
                            cursor,
                            limit,
                        } => {
                            let options = DiscoverOptions {
                                with_genres,
                                without_genres,
                                with_original_language: original_language,
                                min_vote_count: Some(min_vote_count),
                                page: Some(page),
                            };
                            (
                                s.catalog.discover(media_type, &options).await,
                                cursor,
                                limit,
                            )
                        }
                    };
                    let titles = titles.map_err(|e| integration(&e))?;
                    Ok(paginate(
                        titles.into_iter().map(serialize_title).collect(),
                        cursor,
                        limit,
                    ))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_LIBRARY_LIST, {
            let s = s.clone();
            move |input: LibraryListInput, _cx| {
                let s = s.clone();
                async move {
                    let items: Vec<ToolMediaItem> = match input.view {
                        LibraryView::History => require_available(s.library.watch_history().await)?
                            .into_iter()
                            .map(ToolMediaItem::watched)
                            .collect(),
                        LibraryView::InProgress => {
                            require_available(s.library.in_progress().await)?
                                .into_iter()
                                .map(ToolMediaItem::in_progress)
                                .collect()
                        }
                        LibraryView::Library => require_available(s.library.library_index().await)?
                            .into_iter()
                            .map(ToolMediaItem::base)
                            .collect(),
                    };
                    let items = filter_items(items, input.media_type, input.query.as_deref());
                    Ok(paginate(items, input.cursor, input.limit))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_WATCHLIST_LIST, {
            let s = s.clone();
            move |input: WatchlistListInput, _cx| {
                let s = s.clone();
                async move {
                    let items = require_available(s.watchlist.fetch().await)?
                        .into_iter()
                        .map(ToolMediaItem::base)
                        .collect();
                    let items = filter_items(items, input.media_type, input.query.as_deref());
                    Ok(paginate(items, input.cursor, input.limit))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_WATCHLIST_ADD, {
            let s = s.clone();
            move |input: WatchlistAddInput, _cx| {
                let s = s.clone();
                async move {
                    let title = non_empty_trimmed(&input.title, "title")?;
                    let outcome = s
                        .watchlist
                        .add(&WatchlistAddRequest {
                            tmdb_id: input.tmdb_id,
                            media_type: input.media_type,
                            title,
                            year: input.year,
                            external_ids: None,
                        })
                        .await;
                    Ok(WatchlistAddOutput {
                        result: outcome.result.as_str(),
                        title_slug: outcome.title_slug,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_SERIES_GET, {
            let s = s.clone();
            move |input: SeriesGetInput, _cx| {
                let s = s.clone();
                async move {
                    let outcome = s.seasons.status(input.tmdb_id).await;
                    Ok(SeriesGetOutput {
                        result: outcome.result,
                        series: outcome.series,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_SERIES_ADD, {
            let s = s.clone();
            move |input: SeriesAddInput, _cx| {
                let s = s.clone();
                async move {
                    let outcome = s
                        .seasons
                        .add(SeasonAddRequest {
                            tmdb_id: input.tmdb_id,
                            seasons: input.seasons,
                            search: input.search,
                            monitor_new_seasons: input.monitor_new_seasons,
                        })
                        .await;
                    Ok(SeriesAddOutput::from(outcome))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_SERIES_SEASONS_SET, {
            let s = s.clone();
            move |input: SeriesSeasonsSetInput, _cx| {
                let s = s.clone();
                async move {
                    if input.monitor.is_empty()
                        && input.unmonitor.is_empty()
                        && input.monitor_new_seasons.is_none()
                    {
                        return Err(ToolError::input(
                            "monitor, unmonitor or monitorNewSeasons is required",
                        ));
                    }
                    if let Some(season) = input.monitor.intersection(&input.unmonitor).next() {
                        return Err(ToolError::input(format!(
                            "season {season} is in both monitor and unmonitor"
                        )));
                    }
                    let outcome = s
                        .seasons
                        .set(SeasonSetRequest {
                            tmdb_id: input.tmdb_id,
                            monitor: input.monitor,
                            unmonitor: input.unmonitor,
                            monitor_new_seasons: input.monitor_new_seasons,
                        })
                        .await;
                    Ok(SeriesSeasonsSetOutput {
                        result: outcome.result,
                        series: outcome.series,
                        changed_seasons: outcome.changed_seasons,
                        unknown_seasons: outcome.unknown_seasons,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_SERIES_SEARCH, {
            let s = s.clone();
            move |input: SeriesSearchInput, _cx| {
                let s = s.clone();
                async move {
                    let outcome = s
                        .seasons
                        .search(SeasonSearchRequest {
                            tmdb_id: input.tmdb_id,
                            seasons: input.seasons,
                            force: input.force,
                        })
                        .await;
                    Ok(SeriesSearchOutput {
                        result: outcome.result,
                        series: outcome.series,
                        unknown_seasons: outcome.unknown_seasons,
                        seasons: outcome.search,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_SERIES_PROFILE_SET, {
            let s = s.clone();
            move |input: SeriesProfileSetInput, _cx| {
                let s = s.clone();
                async move {
                    let outcome = s
                        .seasons
                        .set_profile(SeriesProfileRequest {
                            tmdb_id: input.tmdb_id,
                            quality_profile_id: input.quality_profile_id,
                            dry_run: input.dry_run,
                        })
                        .await;
                    Ok(SeriesProfileSetOutput {
                        result: outcome.result,
                        previous_quality_profile_id: outcome.previous_quality_profile_id,
                        quality_profile: outcome
                            .quality_profile
                            .map(|(id, name)| ProfileRef { id, name }),
                        series: outcome.series,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_ARR_PROFILES, {
            let s = s.clone();
            move |input: ArrServiceInput, _cx| {
                let s = s.clone();
                async move {
                    let profiles = s
                        .arr
                        .profiles(input.service)
                        .await
                        .map_err(|e| inspect_error(&e))?;
                    Ok(ArrProfilesOutput {
                        service: input.service,
                        profiles,
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_ARR_STATUS, {
            let s = s.clone();
            move |input: ArrServiceInput, _cx| {
                let s = s.clone();
                async move {
                    // Five joined reads: too large to hold inline.
                    Box::pin(s.arr.status(input.service))
                        .await
                        .map_err(|e| inspect_error(&e))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_DOWNLOADS_LIST, {
            let s = s.clone();
            move |input: DownloadsListInput, _cx| {
                let s = s.clone();
                async move {
                    let queue = s
                        .arr
                        .queue(input.service)
                        .await
                        .map_err(|e| inspect_error(&e))?;
                    Ok(DownloadsListOutput {
                        service: input.service,
                        truncated: queue.truncated,
                        page: paginate(queue.downloads, input.cursor, input.limit),
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIES_GET, {
            let s = s.clone();
            move |input: MoviesGetInput, _cx| {
                let s = s.clone();
                async move {
                    if input.detail && input.limit > DETAIL_LIMIT {
                        return Err(ToolError::input(format!(
                            "limit: at most {DETAIL_LIMIT} with detail"
                        )));
                    }
                    s.movies
                        .get(MoviesQuery {
                            tmdb_ids: input.tmdb_ids,
                            query: input.query,
                            cutoff_unmet: input.cutoff_unmet,
                            missing: input.missing,
                            detail: input.detail,
                            cursor: input.cursor,
                            limit: input.limit,
                        })
                        .await
                        .map_err(|e| inspect_error(&e))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIE_COLLECTIONS, {
            let s = s.clone();
            move |input: MovieCollectionsInput, _cx| {
                let s = s.clone();
                async move {
                    let collections = s
                        .movies
                        .collections(input.query.as_deref())
                        .await
                        .map_err(|e| inspect_error(&e))?;
                    Ok(paginate(collections, input.cursor, input.limit))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIES_PROFILE_SET, {
            let s = s.clone();
            move |input: MoviesProfileSetInput, _cx| {
                let s = s.clone();
                async move {
                    Ok(s.movies
                        .set_profile(ProfileSetRequest {
                            tmdb_ids: input.tmdb_ids,
                            quality_profile_id: input.quality_profile_id,
                            monitored: input.monitored,
                            dry_run: input.dry_run,
                        })
                        .await)
                }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIES_SEARCH, {
            let s = s.clone();
            move |input: MoviesSearchInput, _cx| {
                let s = s.clone();
                async move {
                    Ok(s.movies
                        .search(MovieSearchRequest {
                            tmdb_ids: input.tmdb_ids,
                            purpose: input.purpose,
                            force: input.force,
                        })
                        .await)
                }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIE_RELEASES, {
            let s = s.clone();
            move |input: MovieReleasesInput, _cx| {
                let s = s.clone();
                async move { Ok(s.movies.releases(input.tmdb_id).await) }
            }
        })?,
        typed_tool(&defs::MEDIA_MOVIE_RELEASE_GRAB, {
            let s = s.clone();
            move |input: MovieReleaseGrabInput, _cx| {
                let s = s.clone();
                async move {
                    Ok(s.movies
                        .grab(GrabRequest {
                            tmdb_id: input.tmdb_id,
                            release_id: input.release_id,
                            force: input.force,
                        })
                        .await)
                }
            }
        })?,
        typed_tool(&defs::MEDIA_RECOMMENDATIONS_LIST, {
            let s = s.clone();
            move |input: RecommendationsListInput, _cx| {
                let s = s.clone();
                async move {
                    let all = get_all_recommendations(&s.store)
                        .await
                        .map_err(|e| store_error(&e))?;
                    let items = all
                        .iter()
                        .filter(|rec| input.status.is_none_or(|status| rec.status() == status))
                        .filter(|rec| match input.feedback {
                            None => true,
                            Some(FeedbackFilter::None) => rec.feedback.is_none(),
                            Some(FeedbackFilter::GoodPick) => {
                                rec.feedback == Some(RecommendationFeedback::GoodPick)
                            }
                            Some(FeedbackFilter::NotForMe) => {
                                rec.feedback == Some(RecommendationFeedback::NotForMe)
                            }
                            Some(FeedbackFilter::AlreadyWatched) => {
                                rec.feedback == Some(RecommendationFeedback::AlreadyWatched)
                            }
                        })
                        .map(serialize_recommendation)
                        .collect();
                    Ok(paginate(items, input.cursor, input.limit))
                }
            }
        })?,
        typed_tool(&defs::MEDIA_RECOMMENDATION_GET, {
            let s = s.clone();
            move |input: RecommendationGetInput, _cx| {
                let s = s.clone();
                async move {
                    let rec = get_recommendation(&s.store, &input.recommendation_id)
                        .await
                        .map_err(|e| store_error(&e))?
                        .ok_or_else(|| ToolError::execute("Media recommendation not found"))?;
                    Ok(RecommendationOutput {
                        recommendation: serialize_recommendation(&rec),
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_RECOMMENDATION_FEEDBACK, {
            let s = s.clone();
            move |input: RecommendationFeedbackInput, _cx| {
                let s = s.clone();
                async move {
                    if input.feedback.is_none() && input.note.is_none() {
                        return Err(ToolError::input("feedback or note is required"));
                    }
                    let rec = set_recommendation_feedback(
                        &s.store,
                        s.now(),
                        &input.recommendation_id,
                        FeedbackInput {
                            feedback: input.feedback,
                            note: input.note.as_deref().map(|n| trim(n).to_owned()),
                        },
                    )
                    .await
                    .map_err(|e| store_error(&e))?
                    .ok_or_else(|| ToolError::execute("Media recommendation not found"))?;
                    Ok(RecommendationOutput {
                        recommendation: serialize_recommendation(&rec),
                    })
                }
            }
        })?,
        typed_tool(&defs::MEDIA_TASTE_READ, {
            let s = s.clone();
            move |input: TasteReadInput, _cx| {
                let s = s.clone();
                async move {
                    match input {
                        TasteReadInput::Profile {} => {
                            let profile = get_latest_taste_profile(&s.store)
                                .await
                                .map_err(|e| store_error(&e))?;
                            Ok(TasteReadOutput::Profile {
                                profile: profile.map(|p| Box::new(p.profile)),
                            })
                        }
                        TasteReadInput::Evidence { cursor, limit } => {
                            let evidence = get_all_taste_evidence(&s.store)
                                .await
                                .map_err(|e| store_error(&e))?;
                            let items = evidence
                                .into_iter()
                                .map(|item| EvidenceItem {
                                    kind: item.kind.as_str(),
                                    evidence_id: item.evidence_id,
                                    canonical_id: item.canonical_id,
                                    title: item.title,
                                    media_type: item.media_type,
                                    observed_at: item.observed_at,
                                    completion: item.completion,
                                    recommendation_id: item.recommendation_id,
                                    feedback: item.feedback,
                                    note: item.note,
                                })
                                .collect();
                            Ok(TasteReadOutput::Evidence {
                                page: paginate(items, cursor, limit),
                            })
                        }
                    }
                }
            }
        })?,
    ])
}
