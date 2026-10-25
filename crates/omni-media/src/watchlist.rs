//! Radarr (movies) + Sonarr (series) as one watchlist.
//!
//! Either service being unavailable makes the combined watchlist unavailable,
//! so callers never mistake a partial response for the complete tracked state.

use futures::future::BoxFuture;

use crate::arr::movies::RadarrMovies;
use crate::arr::radarr::fetch_radarr_movies;
use crate::arr::seasons::SonarrSeasons;
use crate::arr::sonarr::fetch_sonarr_series;
use crate::arr::{ArrConfig, ArrHttp};
use crate::types::{ExternalIds, FetchResult, MediaItem, MediaType, WatchlistAddOutcome};

/// `WatchlistAddRequest`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchlistAddRequest {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i64>,
    pub external_ids: Option<ExternalIds>,
}

pub trait Watchlist: Send + Sync {
    fn fetch(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>>;
    /// Movies go to Radarr, series to Sonarr; success only once verified.
    fn add<'a>(&'a self, request: &'a WatchlistAddRequest) -> BoxFuture<'a, WatchlistAddOutcome>;
}

/// The production [`Watchlist`].
#[derive(Clone)]
pub struct ArrWatchlist {
    http: ArrHttp,
    radarr: ArrConfig,
    sonarr: ArrConfig,
    seasons: SonarrSeasons,
    movies: RadarrMovies,
}

impl ArrWatchlist {
    /// `seasons` and `movies` record the search an add starts, so the season
    /// and movie tools do not repeat it.
    pub fn new(
        http: ArrHttp,
        radarr: ArrConfig,
        sonarr: ArrConfig,
        seasons: SonarrSeasons,
        movies: RadarrMovies,
    ) -> Self {
        Self {
            http,
            radarr,
            sonarr,
            seasons,
            movies,
        }
    }

    pub fn http(&self) -> &ArrHttp {
        &self.http
    }
}

impl Watchlist for ArrWatchlist {
    fn fetch(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>> {
        Box::pin(async move {
            let (movies, series) = futures::join!(
                fetch_radarr_movies(&self.http, &self.radarr),
                fetch_sonarr_series(&self.http, &self.sonarr)
            );
            match (movies, series) {
                (Some(mut movies), Some(series)) => {
                    movies.extend(series);
                    FetchResult::Ok(movies)
                }
                _ => FetchResult::unavailable("Radarr or Sonarr is unavailable"),
            }
        })
    }

    fn add<'a>(&'a self, request: &'a WatchlistAddRequest) -> BoxFuture<'a, WatchlistAddOutcome> {
        Box::pin(async move {
            match request.media_type {
                MediaType::Movie => {
                    WatchlistAddOutcome::of(self.movies.add_movie(request.tmdb_id).await)
                }
                MediaType::Tv => self.seasons.add_whole_series(request.tmdb_id).await,
            }
        })
    }
}
