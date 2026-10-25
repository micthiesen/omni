//! Radarr movies.

use futures::future::BoxFuture;
use omni_http::{Method, SideEffectMode};
use serde::Deserialize;

use super::movies::AddReservation;
use super::{ArrConfig, ArrHttp, HttpResult, Limits, WriteOutcome, put_opt};
use crate::js::present;
use crate::types::{AddToWatchlistResult, ExternalIds, MediaItem, MediaType};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadarrMovie {
    #[serde(default, deserialize_with = "present")]
    pub id: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub year: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub tmdb_id: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    pub imdb_id: Option<String>,
}

/// The durable record of the search a movie add asks Radarr to run, so the
/// movie search tool never searches that movie again.
pub trait MovieAddLedger: Sync {
    /// Before the POST; `None` (the record failed) aborts the add.
    fn reserve(&self, tmdb_id: i64) -> BoxFuture<'_, Option<AddReservation>>;
    /// After the POST, with what its response proves.
    fn settle(
        &self,
        tmdb_id: i64,
        reservation: AddReservation,
        outcome: WriteOutcome,
    ) -> BoxFuture<'_, ()>;
}

/// Tracked movies, or `None` when Radarr is unavailable or unconfigured.
pub async fn fetch_radarr_movies(http: &ArrHttp, config: &ArrConfig) -> Option<Vec<MediaItem>> {
    let connection = config.connection()?;
    match http
        .send_limited::<Vec<RadarrMovie>>(connection, Method::GET, "movie", None, Limits::LIBRARY)
        .await
    {
        HttpResult::Ok(movies) => Some(movies.into_iter().filter_map(normalize).collect()),
        _ => None,
    }
}

fn normalize(movie: RadarrMovie) -> Option<MediaItem> {
    let (id, title, tmdb) = (movie.id?, movie.title?, movie.tmdb_id?);
    Some(MediaItem {
        guid: format!("radarr:{id}"),
        title,
        year: movie.year,
        media_type: MediaType::Movie,
        external_ids: Some(ExternalIds {
            tmdb: Some(tmdb),
            imdb: movie.imdb_id,
            tvdb: None,
        }),
        title_slug: None,
    })
}

fn tracks(movies: &[MediaItem], tmdb_id: i64) -> bool {
    movies.iter().any(|movie| {
        movie
            .external_ids
            .as_ref()
            .is_some_and(|ids| ids.tmdb == Some(tmdb_id))
    })
}

/// [`add_radarr_movie_with`] without a search record.
pub async fn add_radarr_movie(
    http: &ArrHttp,
    config: &ArrConfig,
    tmdb_id: i64,
) -> AddToWatchlistResult {
    add_radarr_movie_with(http, config, tmdb_id, None).await
}

/// Adds a movie: existing → lookup → reserve the search record → add (search
/// on) → settle the record → verify by TMDB id. Success is reported only once
/// the movie is visible in Radarr's list.
pub async fn add_radarr_movie_with(
    http: &ArrHttp,
    config: &ArrConfig,
    tmdb_id: i64,
    ledger: Option<&dyn MovieAddLedger>,
) -> AddToWatchlistResult {
    let (Some(connection), Some((root_folder_path, quality_profile_id))) =
        (config.connection(), config.acquisition())
    else {
        return AddToWatchlistResult::Unavailable;
    };
    let Some(existing) = fetch_radarr_movies(http, config).await else {
        return AddToWatchlistResult::Unavailable;
    };
    if tracks(&existing, tmdb_id) {
        return AddToWatchlistResult::AlreadyExists;
    }

    let lookup_path = format!(
        "movie/lookup/tmdb?tmdbId={}",
        omni_core::js::encode_uri_component(&tmdb_id.to_string())
    );
    let lookup = match http
        .request_json::<RadarrMovie>(connection, &lookup_path, None)
        .await
    {
        HttpResult::Ok(lookup) => lookup,
        HttpResult::Unavailable => return AddToWatchlistResult::Unavailable,
        HttpResult::HttpError { .. } => return AddToWatchlistResult::Error,
    };
    let (Some(title), Some(lookup_tmdb)) = (
        lookup.title.clone().filter(|t| !t.is_empty()),
        lookup.tmdb_id,
    ) else {
        return AddToWatchlistResult::NotFound;
    };

    let mut body = serde_json::Map::new();
    body.insert("title".to_owned(), title.into());
    put_opt(&mut body, "year", lookup.year);
    body.insert("tmdbId".to_owned(), lookup_tmdb.into());
    put_opt(&mut body, "imdbId", lookup.imdb_id);
    body.insert("qualityProfileId".to_owned(), quality_profile_id.into());
    body.insert("rootFolderPath".to_owned(), root_folder_path.into());
    body.insert("monitored".to_owned(), true.into());
    body.insert(
        "addOptions".to_owned(),
        serde_json::json!({ "searchForMovie": true }),
    );
    let body = serde_json::Value::Object(body);

    if http.mode() == SideEffectMode::Record {
        http.record_write(connection.0, "movie", body);
        return AddToWatchlistResult::Added;
    }
    let reservation = match ledger {
        Some(ledger) => match ledger.reserve(lookup_tmdb).await {
            Some(reservation) => Some((ledger, reservation)),
            None => return AddToWatchlistResult::Error,
        },
        None => None,
    };
    let created = http
        .request_json::<RadarrMovie>(connection, "movie", Some(body))
        .await;
    if let Some((ledger, reservation)) = reservation {
        ledger
            .settle(lookup_tmdb, reservation, WriteOutcome::of(&created))
            .await;
    }
    match created {
        HttpResult::Ok(_) => {}
        HttpResult::Unavailable => return AddToWatchlistResult::Unavailable,
        HttpResult::HttpError { .. } => return AddToWatchlistResult::Error,
    }

    match fetch_radarr_movies(http, config).await {
        None => AddToWatchlistResult::Unavailable,
        Some(verified) if tracks(&verified, tmdb_id) => AddToWatchlistResult::Added,
        Some(_) => AddToWatchlistResult::Error,
    }
}
