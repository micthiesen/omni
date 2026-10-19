//! The media tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use std::collections::BTreeMap;

use omni_mcp_kit::schema::{Lit, Literal, one_of, positive};
use omni_mcp_kit::{Annotations, ExecutorPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

pub static MEDIA_CATALOG_SEARCH: ToolDef<MediaCatalogSearchInput, MediaCatalogSearchOutput> =
    ToolDef::new(ToolInfo {
        name: "media_catalog_search",
        title: "Search Media Catalog",
        description: "Search TMDB for movies or television series. This consumes the configured TMDB API quota but does not change any account.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the public TMDB catalog"],
            cost: "Consumes a small amount of the configured TMDB API quota; no per-call purchase",
            recommended: ExecutorPolicy::Allow,
        },
    });

pub static MEDIA_CATALOG_GET: ToolDef<MediaCatalogGetInput, MediaCatalogGetOutput> = ToolDef::new(
    ToolInfo {
        name: "media_catalog_get",
        title: "Get Media Catalog Details",
        description: "Get bounded TMDB metadata for one movie or television series, including cast, creators, keywords, certification, and viewing commitment.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the public TMDB catalog"],
            cost: "Consumes a small amount of the configured TMDB API quota; no per-call purchase",
            recommended: ExecutorPolicy::Allow,
        },
    },
);

pub static MEDIA_CATALOG_BROWSE: ToolDef<MediaCatalogBrowseInput, MediaCatalogBrowseOutput> =
    ToolDef::new(ToolInfo {
        name: "media_catalog_browse",
        title: "Browse Media Catalog",
        description: "Browse weekly trending titles or a filtered TMDB discovery page. Results are bounded and adult titles are excluded by the underlying client.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the public TMDB catalog"],
            cost: "Consumes a small amount of the configured TMDB API quota; no per-call purchase",
            recommended: ExecutorPolicy::Allow,
        },
    });

pub static MEDIA_LIBRARY_LIST: ToolDef<MediaLibraryListInput, MediaLibraryListOutput> =
    ToolDef::new(ToolInfo {
        name: "media_library_list",
        title: "List Plex Media",
        description: "List the configured Plex library, recent watch history, or in-progress items. An unavailable Plex server is reported as an error, never as an empty library.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Plex account and server"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ExecutorPolicy::Allow,
        },
    });

pub static MEDIA_WATCHLIST_LIST: ToolDef<MediaWatchlistListInput, MediaWatchlistListOutput> =
    ToolDef::new(ToolInfo {
        name: "media_watchlist_list",
        title: "List Managed Watchlist",
        description: "List titles tracked by the configured Radarr and Sonarr services. Either service being unavailable is reported as an error to avoid returning partial state.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Radarr and Sonarr accounts"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ExecutorPolicy::Allow,
        },
    });

pub static MEDIA_WATCHLIST_ADD: ToolDef<MediaWatchlistAddInput, MediaWatchlistAddOutput> =
    ToolDef::new(ToolInfo {
        name: "media_watchlist_add",
        title: "Add to Managed Watchlist",
        description: "Add a TMDB movie to Radarr or a TMDB series to Sonarr. A series is added with every season monitored and an immediate search for all missing episodes; use media_series_add to choose seasons. This can begin acquisition and downloads on managed services, so the owner's approval is required.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Changes the configured Radarr or Sonarr account",
                "May begin media acquisition and downloads",
            ],
            cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
            recommended: ExecutorPolicy::RequireApproval,
        },
    });

pub static MEDIA_SERIES_GET: ToolDef<MediaSeriesGetInput, MediaSeriesGetOutput> = ToolDef::new(
    ToolInfo {
        name: "media_series_get",
        title: "Get Sonarr Series Seasons",
        description: "Inspect one TMDB series in Sonarr: whether it is tracked, which seasons are monitored, per-season episode, file, missing and queued counts, Omni's last search per season, active Sonarr searches, and whether the post-add search of a newly added series is still pending. Use it before changing seasons or searching and to verify the result. An untracked series returns Sonarr's lookup with its season numbers and nothing monitored.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Sonarr account"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ExecutorPolicy::Allow,
        },
    },
);

pub static MEDIA_SERIES_ADD: ToolDef<MediaSeriesAddInput, MediaSeriesAddOutput> = ToolDef::new(
    ToolInfo {
        name: "media_series_add",
        title: "Add Series Seasons to Sonarr",
        description: "Add a TMDB series to Sonarr with only the listed seasons monitored, or, when Sonarr already tracks it, monitor the listed seasons without unmonitoring any other. Monitoring alone downloads only episodes released later. `search: true` also searches the listed seasons once for missing episodes: a new series is searched by Sonarr itself right after its first refresh (outcome `queued_after_add`), an existing one through the same guarded search as media_series_search. Do not call media_series_search afterwards for seasons this call reports as queued, queued_after_add, initial_search_pending, already_running or recently_searched; that would search twice. Later seasons stay unmonitored unless monitorNewSeasons is true. Season 0 holds specials. The result is Sonarr's state read back after the change.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Changes the configured Sonarr account",
                "May begin media acquisition and downloads for the listed seasons",
            ],
            cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
            recommended: ExecutorPolicy::RequireApproval,
        },
    },
);

pub static MEDIA_SERIES_SEASONS_SET: ToolDef<
    MediaSeriesSeasonsSetInput,
    MediaSeriesSeasonsSetOutput,
> = ToolDef::new(ToolInfo {
    name: "media_series_seasons_set",
    title: "Set Sonarr Season Monitoring",
    description: "Monitor or unmonitor specific seasons of a series Sonarr already tracks; seasons not listed keep their state. This never searches: a monitored season downloads only episodes released later, so call media_series_search to fetch episodes that are already missing. Unmonitoring keeps existing files. Monitoring a season of an unmonitored series turns the series on and leaves only the listed seasons monitored. The result is Sonarr's state read back after the change.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Changes the configured Sonarr account",
            "Newly released episodes of monitored seasons may download",
        ],
        cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
        recommended: ExecutorPolicy::RequireApproval,
    },
});

pub static MEDIA_SERIES_SEARCH: ToolDef<MediaSeriesSearchInput, MediaSeriesSearchOutput> =
    ToolDef::new(ToolInfo {
        name: "media_series_search",
        title: "Search Sonarr Seasons",
        description: "Ask Sonarr to search for missing episodes in specific monitored seasons of a tracked series. Each season is searched at most once per 24 hours, including searches started by media_series_add or media_watchlist_add through Omni: a season searched recently, one whose post-add search is still pending, or one with a Sonarr search already running is skipped and reported instead of searched again. Set force only when the user explicitly asks to search again; it bypasses the 24-hour record, including a post-add search whose outcome was uncertain, but never a post-add search Sonarr has not run yet or a Sonarr search still running. Unmonitored seasons and seasons with nothing missing are skipped.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Starts searches on the configured Sonarr account",
                "Begins media acquisition and downloads for the listed seasons",
            ],
            cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
            recommended: ExecutorPolicy::RequireApproval,
        },
    });

pub static MEDIA_RECOMMENDATIONS_LIST: ToolDef<
    MediaRecommendationsListInput,
    MediaRecommendationsListOutput,
> = ToolDef::new(ToolInfo {
    name: "media_recommendations_list",
    title: "List Media Recommendations",
    description: "List persisted media recommendation attempts and outcomes, optionally filtered by status or feedback.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &[],
        cost: "No external traffic or monetary cost",
        recommended: ExecutorPolicy::Allow,
    },
});

pub static MEDIA_RECOMMENDATION_GET: ToolDef<
    MediaRecommendationGetInput,
    MediaRecommendationGetOutput,
> = ToolDef::new(ToolInfo {
    name: "media_recommendation_get",
    title: "Get Media Recommendation",
    description: "Get one persisted media recommendation by its recommendation ID.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &[],
        cost: "No external traffic or monetary cost",
        recommended: ExecutorPolicy::Allow,
    },
});

pub static MEDIA_RECOMMENDATION_FEEDBACK: ToolDef<
    MediaRecommendationFeedbackInput,
    MediaRecommendationFeedbackOutput,
> = ToolDef::new(ToolInfo {
    name: "media_recommendation_feedback",
    title: "Record Media Recommendation Feedback",
    description: "Record a good-pick, not-for-me, or already-watched assessment and/or a bounded note. This changes only Omni's local recommendation state.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
    },
    policy: Policy {
        side_effects: &["Updates local recommendation feedback used by future taste analysis"],
        cost: "No external traffic or monetary cost",
        recommended: ExecutorPolicy::Allow,
    },
});

pub static MEDIA_TASTE_READ: ToolDef<MediaTasteReadInput, MediaTasteReadOutput> = ToolDef::new(
    ToolInfo {
        name: "media_taste_read",
        title: "Read Media Taste Data",
        description: "Read the latest derived media taste profile or paginated evidence rows supporting it.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: false,
        },
        policy: Policy {
            side_effects: &[],
            cost: "No external traffic or monetary cost",
            recommended: ExecutorPolicy::Allow,
        },
    },
);

/// Every media tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 14] = [
    &MEDIA_CATALOG_SEARCH,
    &MEDIA_CATALOG_GET,
    &MEDIA_CATALOG_BROWSE,
    &MEDIA_LIBRARY_LIST,
    &MEDIA_WATCHLIST_LIST,
    &MEDIA_WATCHLIST_ADD,
    &MEDIA_SERIES_GET,
    &MEDIA_SERIES_ADD,
    &MEDIA_SERIES_SEASONS_SET,
    &MEDIA_SERIES_SEARCH,
    &MEDIA_RECOMMENDATIONS_LIST,
    &MEDIA_RECOMMENDATION_GET,
    &MEDIA_RECOMMENDATION_FEEDBACK,
    &MEDIA_TASTE_READ,
];

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum MediaType {
    Movie,
    Tv,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaCatalogSearchInput {
    #[schemars(length(min = 1, max = 200))]
    pub query: String,
    pub media_type: MediaType,
    #[schemars(range(min = 1888, max = 2200))]
    pub year: Option<u64>,
    #[schemars(extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 20), extend("default" = 10))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaCatalogSearchItem {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i64>,
    pub overview: String,
    pub genre_ids: Vec<i64>,
    pub vote_average: f64,
    pub vote_count: u64,
    pub popularity: f64,
    pub poster_path: Option<String>,
    pub original_language: Option<String>,
    #[schemars(url)]
    pub tmdb_url: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaCatalogSearchOutput {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<MediaCatalogSearchItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaCatalogGetInput {
    pub media_type: MediaType,
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Details {
    pub genres: Vec<String>,
    pub runtime_minutes: Option<f64>,
    pub season_count: Option<f64>,
    pub episode_count: Option<f64>,
    pub series_status: Option<String>,
    pub original_language: Option<String>,
    pub origin_countries: Vec<String>,
    pub creators: Vec<String>,
    pub cast: Vec<String>,
    pub keywords: Vec<String>,
    pub certification: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaCatalogGetOutput {
    pub media_type: MediaType,
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(url)]
    pub tmdb_url: String,
    pub details: Details,
}

#[derive(JsonSchema)]
pub struct MediaCatalogBrowseInputTrending {
    #[schemars(transform = Literal("trending"))]
    pub mode: Lit,
    #[schemars(extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 20), extend("default" = 10))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
pub struct WithGenre(#[schemars(transform = positive)] pub u64);

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
pub struct MediaCatalogBrowseInputDiscover {
    #[schemars(transform = Literal("discover"))]
    pub mode: Lit,
    pub media_type: MediaType,
    #[schemars(length(max = 10))]
    pub with_genres: Option<Vec<WithGenre>>,
    #[schemars(length(max = 10))]
    pub without_genres: Option<Vec<WithGenre>>,
    #[schemars(pattern("^[a-z]{2}$"))]
    pub original_language: Option<String>,
    #[schemars(range(max = 100000), extend("default" = 300))]
    pub min_vote_count: Option<u64>,
    #[schemars(range(min = 1, max = 500), extend("default" = 1))]
    pub page: Option<u64>,
    #[schemars(extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 20), extend("default" = 10))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum MediaCatalogBrowseInput {
    Trending(MediaCatalogBrowseInputTrending),
    Discover(MediaCatalogBrowseInputDiscover),
}

pub type MediaCatalogBrowseOutput = MediaCatalogSearchOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum View {
    Library,
    History,
    InProgress,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaLibraryListInput {
    pub view: View,
    pub media_type: Option<MediaType>,
    #[schemars(length(max = 200))]
    pub query: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExternalIds {
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub tvdb: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaLibraryListItem {
    pub guid: String,
    pub title: String,
    pub year: Option<i64>,
    pub media_type: MediaType,
    pub external_ids: Option<ExternalIds>,
    #[schemars(skip_serializing_if = "Option::is_none", range(min = 0, max = 1))]
    pub progress: Option<f64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<i64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub viewed_at: Option<i64>,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub view_count: Option<u64>,
    #[schemars(skip_serializing_if = "Option::is_none", range(min = 0, max = 1))]
    pub completion: Option<f64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaLibraryListOutput {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<MediaLibraryListItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaWatchlistListInput {
    pub media_type: Option<MediaType>,
    #[schemars(length(max = 200))]
    pub query: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

pub type MediaWatchlistListOutput = MediaLibraryListOutput;

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaWatchlistAddInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    pub media_type: MediaType,
    #[schemars(length(min = 1, max = 300))]
    pub title: String,
    #[schemars(range(min = 1888, max = 2200))]
    pub year: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaWatchlistAddResult {
    Added,
    AlreadyExists,
    NotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaWatchlistAddOutput {
    pub result: MediaWatchlistAddResult,
    pub title_slug: Option<String>,
}

#[derive(JsonSchema)]
pub struct SeasonNumber(#[schemars(range(max = 1000))] pub u64);

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesGetInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum SearchVia {
    Add,
    Search,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum SearchState {
    Reserved,
    Queued,
    QueuedAfterAdd,
    Uncertain,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LastSearch {
    pub requested_at: i64,
    pub via: SearchVia,
    #[schemars(
        description = "reserved and uncertain mean the request may have reached Sonarr; they block a repeat like a confirmed search"
    )]
    pub state: SearchState,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeasonStatus {
    pub season_number: u64,
    pub monitored: bool,
    #[schemars(description = "Monitored episodes that have aired, plus episodes with files")]
    pub episode_count: Option<u64>,
    pub episode_file_count: Option<u64>,
    pub total_episode_count: Option<u64>,
    #[schemars(description = "episodeCount minus episodeFileCount")]
    pub missing_episodes: Option<u64>,
    #[schemars(description = "Episodes currently in Sonarr's download queue")]
    pub queued_episodes: u64,
    #[schemars(description = "The last search Omni started or queued for this season")]
    pub last_search: Option<LastSearch>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveSearch {
    pub name: String,
    pub status: String,
    #[schemars(description = "null for a search of the whole series")]
    pub season_number: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeriesStatus {
    pub tracked: bool,
    pub sonarr_id: Option<i64>,
    pub title: String,
    pub year: Option<i64>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: i64,
    pub title_slug: Option<String>,
    #[schemars(description = "Series-level monitoring; a season downloads only when both are on")]
    pub monitored: bool,
    #[schemars(
        description = "Whether seasons that appear later are monitored; null when Sonarr does not report it"
    )]
    pub monitor_new_seasons: Option<bool>,
    #[schemars(description = "Sonarr's search after adding the series has not run yet")]
    pub initial_search_pending: bool,
    #[schemars(description = "Sonarr's first refresh after adding the series has not finished")]
    pub initial_refresh_pending: bool,
    pub seasons: Vec<SeasonStatus>,
    pub active_searches: Vec<ActiveSearch>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaSeriesGetResult {
    Tracked,
    NotTracked,
    NotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesGetOutput {
    pub result: MediaSeriesGetResult,
    pub series: Option<SeriesStatus>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesAddInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(
        description = "Season numbers to monitor; season 0 holds specials",
        length(min = 1, max = 100)
    )]
    pub seasons: Vec<SeasonNumber>,
    #[schemars(
        description = "Also search the listed seasons once for missing episodes. false only monitors them"
    )]
    pub search: bool,
    #[schemars(
        description = "Monitor seasons that appear later. Defaults to false for a new series; unchanged for an existing one when omitted"
    )]
    pub monitor_new_seasons: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum SeasonSearchOutcome {
    Queued,
    QueuedAfterAdd,
    InitialSearchPending,
    RefreshPending,
    AlreadyRunning,
    RecentlySearched,
    NotMonitored,
    NothingMissing,
    Uncertain,
    Failed,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeasonSearchResult {
    pub season_number: u64,
    #[schemars(
        description = "queued, queued_after_add, initial_search_pending, already_running and recently_searched mean a search is covered: do not search the season again. uncertain may have started one and is never retried. refresh_pending means Sonarr is still preparing a series just added without a search; search again in a few minutes"
    )]
    pub outcome: SeasonSearchOutcome,
    pub command_id: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaSeriesAddResult {
    Added,
    Updated,
    Unchanged,
    NotFound,
    SeasonNotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesAddOutput {
    pub result: MediaSeriesAddResult,
    #[schemars(description = "Sonarr's state after the change; null when it could not be read")]
    pub series: Option<SeriesStatus>,
    #[schemars(description = "Seasons whose monitoring this call changed")]
    pub changed_seasons: Vec<u64>,
    #[schemars(description = "Requested seasons the series does not have; nothing is changed")]
    pub unknown_seasons: Vec<u64>,
    #[schemars(description = "Per-season search outcome; empty when search was false")]
    pub search: Vec<SeasonSearchResult>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesSeasonsSetInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(length(max = 100), extend("default" = []))]
    pub monitor: Option<Vec<SeasonNumber>>,
    #[schemars(length(max = 100), extend("default" = []))]
    pub unmonitor: Option<Vec<SeasonNumber>>,
    #[schemars(description = "Monitor seasons that appear later; unchanged when omitted")]
    pub monitor_new_seasons: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaSeriesSeasonsSetResult {
    Updated,
    Unchanged,
    NotTracked,
    NotFound,
    SeasonNotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesSeasonsSetOutput {
    pub result: MediaSeriesSeasonsSetResult,
    pub series: Option<SeriesStatus>,
    pub changed_seasons: Vec<u64>,
    pub unknown_seasons: Vec<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesSearchInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(length(min = 1, max = 100))]
    pub seasons: Vec<SeasonNumber>,
    #[schemars(
        description = "Search even if Omni searched the season in the last 24 hours. Only when the user explicitly asks to search again",
        extend("default" = false)
    )]
    pub force: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaSeriesSearchResult {
    Searched,
    NothingSearched,
    NotTracked,
    NotFound,
    SeasonNotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesSearchOutput {
    pub result: MediaSeriesSearchResult,
    pub series: Option<SeriesStatus>,
    pub unknown_seasons: Vec<u64>,
    pub seasons: Vec<SeasonSearchResult>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Notified,
    Watched,
    Abandoned,
    Ignored,
    Failed,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Feedback {
    GoodPick,
    NotForMe,
    AlreadyWatched,
    None,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaRecommendationsListInput {
    pub status: Option<Status>,
    pub feedback: Option<Feedback>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaRecommendationsListItemFeedback {
    GoodPick,
    NotForMe,
    AlreadyWatched,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRecommendationsListItem {
    pub recommendation_id: String,
    pub canonical_id: String,
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i64>,
    pub status: Status,
    pub why_for_user: Option<String>,
    pub caveats: Vec<String>,
    pub confidence: Option<f64>,
    pub genres: Vec<String>,
    pub runtime_minutes: Option<f64>,
    pub season_count: Option<f64>,
    pub episode_count: Option<f64>,
    pub run_date: String,
    pub recommended_at: i64,
    pub notified_at: Option<f64>,
    pub resolved_at: Option<f64>,
    pub watchlist_result: Option<String>,
    pub feedback: Option<MediaRecommendationsListItemFeedback>,
    pub feedback_at: Option<f64>,
    pub feedback_note: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRecommendationsListOutput {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<MediaRecommendationsListItem>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRecommendationGetInput {
    #[schemars(length(min = 1, max = 200))]
    pub recommendation_id: String,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaRecommendationGetOutput {
    pub recommendation: MediaRecommendationsListItem,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRecommendationFeedbackInput {
    #[schemars(length(min = 1, max = 200))]
    pub recommendation_id: String,
    pub feedback: Option<MediaRecommendationsListItemFeedback>,
    #[schemars(length(max = 1000))]
    pub note: Option<String>,
}

pub type MediaRecommendationFeedbackOutput = MediaRecommendationGetOutput;

#[derive(JsonSchema)]
pub struct MediaTasteReadInputProfile {
    #[schemars(transform = Literal("profile"))]
    pub resource: Lit,
}

#[derive(JsonSchema)]
pub struct MediaTasteReadInputEvidence {
    #[schemars(transform = Literal("evidence"))]
    pub resource: Lit,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum MediaTasteReadInput {
    Profile(MediaTasteReadInputProfile),
    Evidence(MediaTasteReadInputEvidence),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StablePreference {
    pub claim: String,
    pub confidence: f64,
    pub evidence_ids: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recommendations {
    pub total: u64,
    pub watched: u64,
    pub abandoned: u64,
    pub ignored: u64,
    pub failed: u64,
    pub awaiting_outcome: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsCompletedMoviesFeedback {
    pub good_pick: u64,
    pub not_for_me: u64,
    pub already_watched: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourcePerformanceValue {
    pub total: u64,
    pub watched: u64,
    pub good_pick: u64,
    pub not_for_me: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsCompletedMovies {
    pub completed_movies: u64,
    pub completed_series: u64,
    pub rewatched_titles: u64,
    pub recommendations: Recommendations,
    pub feedback: StatsCompletedMoviesFeedback,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub average_hours_to_start: Option<f64>,
    pub source_performance: BTreeMap<String, SourcePerformanceValue>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodesRecommendations {
    pub total: u64,
    pub listened: u64,
    pub abandoned: u64,
    pub ignored: u64,
    pub failed: u64,
    pub awaiting_outcome: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodesFeedback {
    pub good_pick: u64,
    pub not_for_me: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatsListenedEpisodes {
    pub listened_episodes: u64,
    pub started_episodes: u64,
    pub starred_episodes: u64,
    pub distinct_shows: u64,
    pub recommendations: StatsListenedEpisodesRecommendations,
    pub feedback: StatsListenedEpisodesFeedback,
}

#[derive(JsonSchema)]
#[schemars(untagged)]
pub enum Stats {
    CompletedMovies(StatsCompletedMovies),
    ListenedEpisodes(StatsListenedEpisodes),
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Preference {
    Positive,
    Neutral,
    Negative,
    Uncertain,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Movies {
    pub preference: Preference,
    pub confidence: f64,
    pub evidence_ids: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitmentPreferences {
    pub movies: Movies,
    pub limited_series: Movies,
    pub long_series: Movies,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub profile_id: String,
    pub version: i64,
    pub generated_at: i64,
    pub evidence_fingerprint: String,
    pub evidence_count: u64,
    pub model_id: String,
    pub prompt_version: String,
    pub summary: String,
    pub stable_preferences: Vec<StablePreference>,
    pub conditional_preferences: Vec<StablePreference>,
    pub aversions: Vec<StablePreference>,
    pub current_saturation: Vec<StablePreference>,
    pub exploration_targets: Vec<StablePreference>,
    pub uncertainties: Vec<StablePreference>,
    pub stats: Stats,
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub commitment_preferences: Option<CommitmentPreferences>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaTasteReadOutputProfile {
    #[schemars(transform = Literal("profile"))]
    pub resource: Lit,
    pub profile: Option<Profile>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum Kind {
    PlexWatch,
    RecommendationOutcome,
    ExplicitFeedback,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaTasteReadOutputEvidenceItem {
    pub evidence_id: String,
    pub kind: Kind,
    pub canonical_id: String,
    pub title: String,
    pub media_type: MediaType,
    pub observed_at: i64,
    pub completion: Option<f64>,
    pub recommendation_id: Option<String>,
    pub feedback: Option<MediaRecommendationsListItemFeedback>,
    pub note: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaTasteReadOutputEvidence {
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(transform = Literal("evidence"))]
    pub resource: Lit,
    pub items: Vec<MediaTasteReadOutputEvidenceItem>,
}

#[derive(JsonSchema)]
#[schemars(untagged, transform = one_of)]
pub enum MediaTasteReadOutput {
    Profile(Box<MediaTasteReadOutputProfile>),
    Evidence(MediaTasteReadOutputEvidence),
}
