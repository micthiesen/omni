//! The media tools' contracts: metadata, policy and the schema types
//! their input and output schemas derive from (`omni_mcp_kit::schema`). These
//! types describe the wire format only; the handlers decode and encode with
//! their own types. After changing anything here run `cargo xtask mcp-golden`
//! and review the snapshot diff.

use std::collections::BTreeMap;

use omni_mcp_kit::schema::{Lit, Literal, one_of, positive};
use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::RequireApproval,
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
            recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::RequireApproval,
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
        recommended: ApprovalPolicy::RequireApproval,
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
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static MEDIA_SERIES_PROFILE_SET: ToolDef<
    MediaSeriesProfileSetInput,
    MediaSeriesProfileSetOutput,
> = ToolDef::new(ToolInfo {
    name: "media_series_profile_set",
    title: "Set Sonarr Series Quality Profile",
    description: "Change the quality profile of a series Sonarr already tracks; monitoring is unchanged. Use media_arr_profiles with service sonarr for the profile ids. dryRun previews the change without writing. This never searches: Sonarr grabs upgrades for monitored episodes below the new profile's cutoff as new releases appear. The result is Sonarr's state read back after the change.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Changes the configured Sonarr account",
            "Monitored episodes below the new profile's cutoff may download upgrades as releases appear",
        ],
        cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static MEDIA_ARR_PROFILES: ToolDef<MediaArrProfilesInput, MediaArrProfilesOutput> =
    ToolDef::new(ToolInfo {
        name: "media_arr_profiles",
        title: "List Radarr or Sonarr Quality Profiles",
        description: "List the quality profiles configured in Radarr or Sonarr, read live: id, name, whether upgrades are allowed, the cutoff quality, allowed qualities from least to most preferred, the highest allowed resolution, custom-format score thresholds and nonzero custom-format scores. isDefault marks the profile Omni uses when it adds a title. Use the ids with media_movies_profile_set or media_series_profile_set.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Radarr or Sonarr account"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static MEDIA_ARR_STATUS: ToolDef<MediaArrStatusInput, MediaArrStatusOutput> = ToolDef::new(
    ToolInfo {
        name: "media_arr_status",
        title: "Get Radarr or Sonarr Status",
        description: "Read Radarr's or Sonarr's version, health warnings, root folders with free space, the import settings that decide what an upgrade does with the replaced file (recycle bin, hardlinks, propers), and its download clients. Check it before large upgrade batches. No credentials or client addresses are returned.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Radarr or Sonarr account"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static MEDIA_DOWNLOADS_LIST: ToolDef<MediaDownloadsListInput, MediaDownloadsListOutput> =
    ToolDef::new(ToolInfo {
        name: "media_downloads_list",
        title: "List Radarr or Sonarr Downloads",
        description: "List the Radarr or Sonarr download queue: progress, time left, the tracked state (downloading, import pending, import blocked, failed), and the messages explaining a stalled, rejected or failed import. Downloads the service could not match to a title are included.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Radarr or Sonarr account"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static MEDIA_MOVIES_GET: ToolDef<MediaMoviesGetInput, MediaMoviesGetOutput> = ToolDef::new(
    ToolInfo {
        name: "media_movies_get",
        title: "Get Radarr Movies",
        description: "Inspect movies Radarr tracks, by TMDB ids or by a title or collection-name query (for example \"Star Wars\"): monitoring, quality profile, whether the file is below the profile's cutoff, the file's quality, resolution, codecs, HDR and audio, downloads in the queue, active Radarr searches, Omni's last search or grab, and the upgrade it is waiting for. A movie without a file has file null. detail adds custom formats, Radarr's recent history, and a scan of the movie's folder that lists every managed and unmanaged video file; an upgrade is verified only when its file changed and it is the only video file left. Use it to plan and to verify profile changes, searches and upgrades.",
        annotations: Annotations {
            read_only_hint: true,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Reads the configured Radarr account"],
            cost: "No expected monetary cost; bounded account API traffic",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static MEDIA_MOVIE_COLLECTIONS: ToolDef<
    MediaMovieCollectionsInput,
    MediaMovieCollectionsOutput,
> = ToolDef::new(ToolInfo {
    name: "media_movie_collections",
    title: "List Radarr Collections",
    description: "List Radarr's TMDB collections whose name or a member's title contains the query, with every member and whether Radarr tracks it, to find collection gaps. Use media_movies_get for the tracked members' files and quality, and media_watchlist_add to add a missing one.",
    annotations: Annotations {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &["Reads the configured Radarr account"],
        cost: "No expected monetary cost; bounded account API traffic",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static MEDIA_MOVIES_PROFILE_SET: ToolDef<
    MediaMoviesProfileSetInput,
    MediaMoviesProfileSetOutput,
> = ToolDef::new(ToolInfo {
    name: "media_movies_profile_set",
    title: "Set Radarr Movie Quality Profile",
    description: "Change the quality profile, and optionally monitoring, of movies Radarr tracks, by TMDB id. Use media_arr_profiles with service radarr for the profile ids. dryRun returns the current and planned profile of every movie without writing; show it to the user before applying. Only the listed movies change, and each is read back from Radarr afterwards. This never searches: a monitored movie below the new cutoff picks up upgrades as new releases appear, and media_movies_search with purpose upgrade searches existing releases once.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Changes the configured Radarr account",
            "Monitored movies below the new profile's cutoff may download upgrades as releases appear",
        ],
        cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
        recommended: ApprovalPolicy::RequireApproval,
    },
});

pub static MEDIA_MOVIES_SEARCH: ToolDef<MediaMoviesSearchInput, MediaMoviesSearchOutput> =
    ToolDef::new(ToolInfo {
        name: "media_movies_search",
        title: "Search Radarr Movies",
        description: "Ask Radarr to run its automatic search, which picks and downloads the best release its profile and rules accept, for the listed movies. purpose missing searches only movies without a file; purpose upgrade searches only movies whose file is below the profile's quality cutoff (a file that meets it by quality but not by custom-format score is reported cutoff_met; Radarr's RSS still upgrades it as better releases appear), and Radarr replaces the old file itself when the upgrade imports, so nothing is deleted first. Each movie is searched at most once per 24 hours, counting searches, grabs and adds through Omni: a movie searched or grabbed recently, one with a download in the queue, a Radarr search running, a grab in Radarr's history within 24 hours, or a post-add search not yet run is skipped and reported instead. Set force only when the user explicitly asks to search again; it bypasses only the 24-hour checks, including an earlier uncertain request and a grab in Radarr's history, never a queued download, a running search or a pending post-add search. Returns Radarr's command ids; a queued search is not a download, so verify with media_movies_get.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &[
                "Starts searches on the configured Radarr account",
                "Begins media acquisition and downloads; an imported upgrade replaces the old file",
            ],
            cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static MEDIA_MOVIE_RELEASES: ToolDef<MediaMovieReleasesInput, MediaMovieReleasesOutput> =
    ToolDef::new(ToolInfo {
        name: "media_movie_releases",
        title: "List Radarr Movie Releases",
        description: "Run Radarr's interactive search for one tracked movie and list the releases its indexers return, approved ones first in Radarr's preference order: quality, resolution, size, custom formats and score, languages, release group, age, and Radarr's rejection reasons. Nothing is grabbed. Prefer media_movies_search; use this only when a title needs scrutiny (a bad automatic pick, a quality problem, HDR or audio requirements), then grab one approved release with media_movie_release_grab and explain the choice. Each call queries every indexer, so do not repeat it needlessly.",
        annotations: Annotations {
            read_only_hint: false,
            destructive_hint: false,
            idempotent_hint: true,
            open_world_hint: true,
        },
        policy: Policy {
            side_effects: &["Queries every configured indexer through Radarr; nothing is grabbed"],
            cost: "Consumes indexer API quota; no download",
            recommended: ApprovalPolicy::Allow,
        },
    });

pub static MEDIA_MOVIE_RELEASE_GRAB: ToolDef<
    MediaMovieReleaseGrabInput,
    MediaMovieReleaseGrabOutput,
> = ToolDef::new(ToolInfo {
    name: "media_movie_release_grab",
    title: "Grab a Radarr Movie Release",
    description: "Download one release chosen from media_movie_releases (by its releaseId) for a tracked movie. Omni searches again first and grabs only when the same release is still offered and Radarr approves it under the movie's profile and release rules; a rejected release is never grabbed. A movie with a download in the queue, a Radarr search running, a post-add search not yet run, a grab within 24 hours (through Omni or in Radarr's history), or an Omni search whose outcome is uncertain is reported instead; a search Radarr accepted that found nothing acceptable does not block it. force bypasses only the 24-hour checks and only when the user explicitly asks. Radarr replaces the old file itself when the download imports.",
    annotations: Annotations {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: true,
    },
    policy: Policy {
        side_effects: &[
            "Sends a release to the download client of the configured Radarr account",
            "An imported upgrade replaces the movie's old file",
        ],
        cost: "No direct API charge; may consume storage, bandwidth, and provider resources",
        recommended: ApprovalPolicy::RequireApproval,
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
        recommended: ApprovalPolicy::Allow,
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
        recommended: ApprovalPolicy::Allow,
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
        recommended: ApprovalPolicy::Allow,
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
            recommended: ApprovalPolicy::Allow,
        },
    },
);

/// Every media tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 24] = [
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
    &MEDIA_SERIES_PROFILE_SET,
    &MEDIA_ARR_PROFILES,
    &MEDIA_ARR_STATUS,
    &MEDIA_DOWNLOADS_LIST,
    &MEDIA_MOVIES_GET,
    &MEDIA_MOVIE_COLLECTIONS,
    &MEDIA_MOVIES_PROFILE_SET,
    &MEDIA_MOVIES_SEARCH,
    &MEDIA_MOVIE_RELEASES,
    &MEDIA_MOVIE_RELEASE_GRAB,
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
    #[schemars(description = "null for a series Sonarr does not track; see media_arr_profiles")]
    pub quality_profile_id: Option<i64>,
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
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesProfileSetInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(transform = positive)]
    pub quality_profile_id: u64,
    #[schemars(
        description = "Return the planned change without writing",
        extend("default" = false)
    )]
    pub dry_run: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaSeriesProfileSetResult {
    Preview,
    Updated,
    Unchanged,
    NotTracked,
    NotFound,
    ProfileNotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ProfileRef {
    pub id: i64,
    pub name: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaSeriesProfileSetOutput {
    pub result: MediaSeriesProfileSetResult,
    pub previous_quality_profile_id: Option<i64>,
    pub quality_profile: Option<ProfileRef>,
    #[schemars(description = "Sonarr's state after the change; null when it could not be read")]
    pub series: Option<SeriesStatus>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum ArrService {
    Radarr,
    Sonarr,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaArrProfilesInput {
    pub service: ArrService,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct FormatScore {
    pub name: String,
    pub score: i64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArrProfile {
    pub id: i64,
    pub name: String,
    pub upgrade_allowed: bool,
    #[schemars(description = "The quality or quality group at which upgrades by quality stop")]
    pub cutoff: Option<String>,
    #[schemars(description = "Allowed qualities, least to most preferred")]
    pub allowed_qualities: Vec<String>,
    pub max_resolution: Option<i64>,
    pub min_format_score: Option<i64>,
    #[schemars(description = "Upgrades continue until a release reaches this custom-format score")]
    pub cutoff_format_score: Option<i64>,
    pub min_upgrade_format_score: Option<i64>,
    #[schemars(description = "Custom formats with a nonzero score")]
    pub custom_format_scores: Vec<FormatScore>,
    pub language: Option<String>,
    #[schemars(description = "The profile Omni uses when it adds a title")]
    pub is_default: bool,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaArrProfilesOutput {
    pub service: ArrService,
    pub profiles: Vec<ArrProfile>,
}

pub type MediaArrStatusInput = MediaArrProfilesInput;

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct HealthItem {
    pub source: Option<String>,
    #[schemars(rename = "type")]
    pub kind: Option<String>,
    pub message: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootFolder {
    pub path: String,
    pub accessible: bool,
    pub free_space: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportSettings {
    #[schemars(description = "null: a file replaced by an upgrade is deleted, not recycled")]
    pub recycle_bin: Option<String>,
    pub copy_using_hardlinks: Option<bool>,
    pub download_propers_and_repacks: Option<String>,
    pub minimum_free_space_when_importing_mb: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct DownloadClient {
    pub name: String,
    pub implementation: Option<String>,
    pub protocol: Option<String>,
    pub enabled: bool,
    pub remove_completed_downloads: Option<bool>,
    pub remove_failed_downloads: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaArrStatusOutput {
    pub service: ArrService,
    pub version: Option<String>,
    pub health: Vec<HealthItem>,
    pub root_folders: Vec<RootFolder>,
    pub import_settings: ImportSettings,
    pub download_clients: Vec<DownloadClient>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaDownloadsListInput {
    pub service: ArrService,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Download {
    pub queue_id: Option<i64>,
    pub title: Option<String>,
    #[schemars(description = "Radarr's movie id or Sonarr's series id; null when unmatched")]
    pub item_id: Option<i64>,
    pub item_title: Option<String>,
    pub tmdb_id: Option<i64>,
    pub tvdb_id: Option<i64>,
    pub season_number: Option<i64>,
    pub episode_number: Option<i64>,
    pub status: Option<String>,
    #[schemars(description = "ok, warning or error")]
    pub tracked_download_status: Option<String>,
    #[schemars(
        description = "For example downloading, importPending, importBlocked, failedPending"
    )]
    pub tracked_download_state: Option<String>,
    #[schemars(description = "Why the download is stuck, rejected or failed")]
    pub messages: Vec<String>,
    pub quality: Option<String>,
    pub custom_format_score: Option<i64>,
    pub size_bytes: Option<f64>,
    pub size_left_bytes: Option<f64>,
    pub time_left: Option<String>,
    pub estimated_completion_time: Option<String>,
    pub added: Option<String>,
    pub protocol: Option<String>,
    pub download_client: Option<String>,
    pub indexer: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaDownloadsListOutput {
    pub service: ArrService,
    #[schemars(description = "The service holds more downloads than one read returns")]
    pub truncated: bool,
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<Download>,
}

#[derive(JsonSchema)]
pub struct TmdbId(#[schemars(transform = positive)] pub u64);

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMoviesGetInput {
    #[schemars(length(min = 1, max = 100))]
    pub tmdb_ids: Option<Vec<TmdbId>>,
    #[schemars(
        description = "Case-insensitive substring of the movie title or its collection's name",
        length(max = 200)
    )]
    pub query: Option<String>,
    #[schemars(
        description = "Only movies whose file is below the profile's quality cutoff",
        extend("default" = false)
    )]
    pub cutoff_unmet: Option<bool>,
    #[schemars(description = "Only movies without a file", extend("default" = false))]
    pub missing: Option<bool>,
    #[schemars(
        description = "Add custom formats, recent history and a scan of each movie's folder; at most 25 movies per page",
        extend("default" = false)
    )]
    pub detail: Option<bool>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 100), extend("default" = 25))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum MovieSearchVia {
    Add,
    Search,
    Grab,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum SearchPurpose {
    Missing,
    Upgrade,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LastMovieSearch {
    pub requested_at: i64,
    pub via: MovieSearchVia,
    pub purpose: Option<SearchPurpose>,
    #[schemars(
        description = "reserved and uncertain mean the request may have reached Radarr; they block a repeat like a confirmed search"
    )]
    pub state: SearchState,
    pub command_id: Option<i64>,
    #[schemars(description = "The movie's file when the search or grab was asked for")]
    pub previous_file_id: Option<i64>,
    pub release_title: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovieFile {
    pub id: Option<i64>,
    pub relative_path: Option<String>,
    pub size_bytes: Option<f64>,
    pub date_added: Option<String>,
    #[schemars(description = "Radarr's quality name, for example Bluray-2160p")]
    pub quality: Option<String>,
    #[schemars(description = "The quality's resolution: 2160 is 4K")]
    pub resolution: Option<i64>,
    pub source: Option<String>,
    #[schemars(description = "The video stream's frame size, for example 3840x1600")]
    pub video_resolution: Option<String>,
    pub video_codec: Option<String>,
    pub video_bit_depth: Option<i64>,
    pub dynamic_range: Option<String>,
    #[schemars(description = "For example HDR10, HDR10Plus, DV HDR10")]
    pub dynamic_range_type: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<f64>,
    pub audio_languages: Option<String>,
    pub run_time: Option<String>,
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    pub scene_name: Option<String>,
    pub edition: Option<String>,
    #[schemars(description = "Present only with detail")]
    pub custom_formats: Option<Vec<String>>,
    pub custom_format_score: Option<i64>,
    #[schemars(description = "The file is below the profile's quality cutoff")]
    pub cutoff_not_met: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ActiveMovieSearch {
    pub name: String,
    pub status: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum UpgradeState {
    Pending,
    Downloading,
    Imported,
    Verified,
    ExtraFiles,
    FileMissing,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpgradeCheck {
    #[schemars(
        description = "pending: nothing grabbed or imported yet. downloading: in the queue. imported: the file changed (pass detail to verify). verified: the file changed and it is the only video file for the movie. extra_files: other video files remain, so the upgrade is not complete. file_missing: the movie has no file"
    )]
    pub state: UpgradeState,
    pub previous_file_id: i64,
    pub current_file_id: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedFile {
    pub id: Option<i64>,
    pub relative_path: Option<String>,
    pub size_bytes: Option<f64>,
    pub quality: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnmanagedFile {
    pub path: Option<String>,
    pub size_bytes: Option<f64>,
    pub quality: Option<String>,
    pub rejections: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LibraryFiles {
    #[schemars(description = "Files Radarr manages for the movie")]
    pub managed: Vec<ManagedFile>,
    #[schemars(
        description = "Other video files in the movie's folder; null when the folder could not be scanned"
    )]
    pub unmanaged: Option<Vec<UnmanagedFile>>,
    #[schemars(description = "Exactly one managed file and no other video file in the folder")]
    pub exactly_one_file: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct HistoryEvent {
    pub event_type: Option<String>,
    pub date: Option<String>,
    pub source_title: Option<String>,
    pub quality: Option<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
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
    #[schemars(description = "null when the movie has no file")]
    pub file: Option<MovieFile>,
    pub managed_file_count: Option<i64>,
    #[schemars(description = "Radarr's search after adding the movie has not run yet")]
    pub initial_search_pending: bool,
    pub downloads: Vec<Download>,
    pub active_searches: Vec<ActiveMovieSearch>,
    #[schemars(description = "The last search or grab Omni started or asked for")]
    pub last_search: Option<LastMovieSearch>,
    #[schemars(description = "The last upgrade Omni asked for, checked against Radarr now")]
    pub upgrade: Option<UpgradeCheck>,
    #[schemars(description = "Present only with detail")]
    pub library_files: Option<LibraryFiles>,
    #[schemars(description = "Present only with detail: Radarr's latest events, newest first")]
    pub history: Option<Vec<HistoryEvent>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMoviesGetOutput {
    pub items: Vec<MovieStatus>,
    pub next_cursor: Option<u64>,
    pub total: u64,
    #[schemars(description = "Requested TMDB ids Radarr does not track")]
    pub not_tracked: Vec<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMovieCollectionsInput {
    #[schemars(
        description = "Case-insensitive substring of the collection's name or a member's title",
        length(max = 200)
    )]
    pub query: Option<String>,
    #[schemars(description = "Zero-based result offset", extend("default" = 0))]
    pub cursor: Option<u64>,
    #[schemars(range(min = 1, max = 50), extend("default" = 10))]
    pub limit: Option<u64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct CollectionMember {
    pub tmdb_id: Option<i64>,
    pub title: Option<String>,
    pub year: Option<i64>,
    #[schemars(description = "Radarr tracks the movie")]
    pub tracked: bool,
    #[schemars(description = "On Radarr's import exclusion list")]
    pub excluded: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovieCollection {
    pub tmdb_id: Option<i64>,
    pub title: String,
    #[schemars(description = "Radarr adds new members automatically")]
    pub monitored: bool,
    pub tracked: u64,
    pub untracked: u64,
    pub members: Vec<CollectionMember>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMovieCollectionsOutput {
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub items: Vec<MovieCollection>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMoviesProfileSetInput {
    #[schemars(length(min = 1, max = 100))]
    pub tmdb_ids: Vec<TmdbId>,
    #[schemars(transform = positive)]
    pub quality_profile_id: u64,
    #[schemars(description = "Also set monitoring; unchanged when omitted")]
    pub monitored: Option<bool>,
    #[schemars(
        description = "Return the current and planned settings without writing",
        extend("default" = false)
    )]
    pub dry_run: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaMoviesProfileSetResult {
    Preview,
    Updated,
    Unchanged,
    NotTracked,
    ProfileNotFound,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovieSettings {
    pub quality_profile_id: Option<i64>,
    pub monitored: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileChange {
    pub tmdb_id: i64,
    pub radarr_id: i64,
    pub title: String,
    pub before: MovieSettings,
    #[schemars(description = "Planned in a preview; Radarr's state read back otherwise")]
    pub after: MovieSettings,
    pub changed: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMoviesProfileSetOutput {
    #[schemars(description = "error: Radarr refused the change or did not apply all of it")]
    pub result: MediaMoviesProfileSetResult,
    pub profile: Option<ProfileRef>,
    pub movies: Vec<ProfileChange>,
    pub not_tracked: Vec<i64>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMoviesSearchInput {
    #[schemars(length(min = 1, max = 50))]
    pub tmdb_ids: Vec<TmdbId>,
    #[schemars(
        description = "missing: only movies without a file. upgrade: only movies whose file is below the profile's cutoff"
    )]
    pub purpose: SearchPurpose,
    #[schemars(
        description = "Search even if the movie was searched or grabbed in the last 24 hours. Only when the user explicitly asks to search again",
        extend("default" = false)
    )]
    pub force: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaMoviesSearchResult {
    Searched,
    NothingSearched,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MovieSearchOutcome {
    Queued,
    Uncertain,
    Failed,
    NotTracked,
    NotMonitored,
    HasFile,
    NoFile,
    CutoffMet,
    UpgradesDisabled,
    InitialSearchPending,
    AlreadyRunning,
    Downloading,
    RecentlySearched,
    RecentlyGrabbed,
    Unavailable,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovieSearchResult {
    pub tmdb_id: i64,
    pub radarr_id: Option<i64>,
    pub title: Option<String>,
    #[schemars(
        description = "queued, initial_search_pending, already_running, downloading, recently_searched and recently_grabbed mean a search is covered: do not search the movie again. uncertain may have started one and is never retried. failed: Radarr rejected the search. unavailable: Radarr's history for the movie could not be read, so nothing was searched"
    )]
    pub outcome: MovieSearchOutcome,
    pub command_id: Option<i64>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaMoviesSearchOutput {
    pub result: MediaMoviesSearchResult,
    pub movies: Vec<MovieSearchResult>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMovieReleasesInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum MediaMovieReleasesResult {
    Listed,
    NotTracked,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    #[schemars(description = "Omni's handle for the release; pass it to media_movie_release_grab")]
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
    #[schemars(description = "Radarr would grab it under the movie's profile and release rules")]
    pub approved: bool,
    pub rejections: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MediaMovieReleasesOutput {
    pub result: MediaMovieReleasesResult,
    pub movie: Option<MovieStatus>,
    #[schemars(
        description = "At most 50: approved releases first, each group in Radarr's preference order"
    )]
    pub releases: Vec<Release>,
    pub total: u64,
    pub approved: u64,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMovieReleaseGrabInput {
    #[schemars(transform = positive)]
    pub tmdb_id: u64,
    #[schemars(
        description = "releaseId from media_movie_releases",
        pattern("^[0-9a-f]{32}$")
    )]
    pub release_id: String,
    #[schemars(
        description = "Grab even if the movie was searched or grabbed in the last 24 hours. Only when the user explicitly asks",
        extend("default" = false)
    )]
    pub force: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "snake_case")]
pub enum GrabOutcome {
    Grabbed,
    Uncertain,
    Failed,
    NotTracked,
    ReleaseNotFound,
    ReleaseRejected,
    InitialSearchPending,
    AlreadyRunning,
    Downloading,
    RecentlyGrabbed,
    RecentlySearched,
    Unavailable,
    Error,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaMovieReleaseGrabOutput {
    #[schemars(
        description = "grabbed: sent to the download client; verify the import with media_movies_get. uncertain may have grabbed and is never retried. release_not_found: the release is no longer offered; list releases again"
    )]
    pub outcome: GrabOutcome,
    pub radarr_id: Option<i64>,
    pub title: Option<String>,
    pub release: Option<Release>,
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
