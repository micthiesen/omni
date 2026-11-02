//! TMDB catalog client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_http::{HttpClient, HttpError, Method, Url};
use serde::de::DeserializeOwned;

use crate::error::IntegrationError;
use crate::types::MediaType;

pub mod types;

use types::{
    Details, FindResponse, GenreList, MovieDetails, MovieList, MovieResult, TmdbTitle,
    TmdbTitleDetails, TrendingList, TrendingResult, TvDetails, TvList, TvResult, normalize_movie,
    normalize_movie_details, normalize_tv, normalize_tv_details,
};

pub const BASE_URL: &str = "https://api.themoviedb.org/3";
const TMDB_TIMEOUT: Duration = Duration::from_secs(15);
const TMDB_MAX_BYTES: usize = 16 * 1024 * 1024;
const TMDB_RETRIES: u32 = 2;
const TMDB_RETRY_BASE: Duration = Duration::from_millis(200);

/// `https://www.themoviedb.org/<mediaType>/<tmdbId>`.
pub fn tmdb_url(media_type: MediaType, tmdb_id: i64) -> String {
    format!(
        "https://www.themoviedb.org/{}/{tmdb_id}",
        media_type.as_str()
    )
}

/// `/find` external id sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindSource {
    Imdb,
    Tvdb,
}

impl FindSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FindSource::Imdb => "imdb_id",
            FindSource::Tvdb => "tvdb_id",
        }
    }
}

/// `DiscoverOptions`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverOptions {
    pub with_genres: Option<Vec<i64>>,
    pub without_genres: Option<Vec<i64>>,
    pub with_original_language: Option<String>,
    pub min_vote_count: Option<i64>,
    pub page: Option<i64>,
}

pub type CatalogResult<T> = Result<T, IntegrationError>;
pub type GenreMap = Arc<HashMap<i64, String>>;

/// The TMDB catalog as the recommendation code uses it.
pub trait Catalog: Send + Sync {
    fn search_titles<'a>(
        &'a self,
        query: &'a str,
        media_type: MediaType,
        year: Option<i64>,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>>;
    fn find_by_external_id<'a>(
        &'a self,
        external_id: &'a str,
        source: FindSource,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>>;
    fn recommendations_for(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>>;
    fn discover<'a>(
        &'a self,
        media_type: MediaType,
        options: &'a DiscoverOptions,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>>;
    fn trending(&self) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>>;
    fn title_genre_ids(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<i64>>>;
    fn title_details(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<TmdbTitleDetails>>;
    fn genre_map(&self, media_type: MediaType) -> BoxFuture<'_, CatalogResult<GenreMap>>;
    /// One title by its exact TMDB id; adult titles are not found.
    fn title(&self, media_type: MediaType, tmdb_id: i64)
    -> BoxFuture<'_, CatalogResult<TmdbTitle>>;
}

/// The production TMDB client; the genre maps are cached for the process.
pub struct TmdbClient {
    http: HttpClient,
    api_key: Option<String>,
    genre_cache: Mutex<HashMap<MediaType, GenreMap>>,
}

impl TmdbClient {
    pub fn new(http: HttpClient, api_key: Option<String>) -> Self {
        Self {
            http,
            api_key: api_key.filter(|k| !k.is_empty()),
            genre_cache: Mutex::new(HashMap::new()),
        }
    }

    /// V4 read tokens (JWTs) as bearer, v3 keys in the query; up to
    /// two retries with exponential backoff for 429/5xx and network failures.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> CatalogResult<T> {
        let key = self.api_key.as_deref().ok_or_else(|| {
            IntegrationError::new("read TMDB API key", "TMDB_API_KEY is not configured")
        })?;
        let bearer = key.starts_with("eyJ");
        let mut url = Url::parse(&format!("{BASE_URL}{path}"))
            .map_err(|e| IntegrationError::new(format!("TMDB GET {path}"), e))?;
        {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in &params {
                pairs.append_pair(name, value);
            }
            if !bearer {
                pairs.append_pair("api_key", key);
            }
        }
        if url.query() == Some("") {
            url.set_query(None);
        }
        let mut attempt = 0u32;
        let raw = loop {
            let mut request = self
                .http
                .request(Method::GET, url.clone())
                .header("Accept", "application/json")
                .timeout(TMDB_TIMEOUT);
            if bearer {
                request = request.bearer_auth(key);
            }
            match request
                .json_bounded::<serde_json::Value>(TMDB_MAX_BYTES)
                .await
            {
                Ok(raw) => break raw,
                Err(error) if attempt < TMDB_RETRIES && is_transient(&error) => {
                    tokio::time::sleep(TMDB_RETRY_BASE * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                Err(error) => return Err(IntegrationError::new(format!("TMDB GET {path}"), error)),
            }
        };
        serde_json::from_value(raw)
            .map_err(|e| IntegrationError::new(format!("decode TMDB {path}"), e))
    }

    async fn movie_list(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> CatalogResult<Vec<TmdbTitle>> {
        let list: MovieList = self.get(path, params).await?;
        Ok(list
            .results
            .into_iter()
            .filter(|r| !r.adult)
            .map(normalize_movie)
            .collect())
    }

    async fn tv_list(
        &self,
        path: &str,
        params: Vec<(&str, String)>,
    ) -> CatalogResult<Vec<TmdbTitle>> {
        let list: TvList = self.get(path, params).await?;
        Ok(list
            .results
            .into_iter()
            .filter(|r| !r.adult)
            .map(normalize_tv)
            .collect())
    }
}

/// 429, 5xx, network failures and timeouts.
fn is_transient(error: &HttpError) -> bool {
    match error {
        HttpError::Timeout | HttpError::Network(_) => true,
        HttpError::Status { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

fn join_ids(ids: &[i64]) -> String {
    ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

impl Catalog for TmdbClient {
    fn search_titles<'a>(
        &'a self,
        query: &'a str,
        media_type: MediaType,
        year: Option<i64>,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        Box::pin(async move {
            let mut params = vec![
                ("query", query.to_owned()),
                ("include_adult", "false".to_owned()),
            ];
            let year = year.filter(|y| *y != 0);
            match media_type {
                MediaType::Movie => {
                    if let Some(year) = year {
                        params.push(("year", year.to_string()));
                    }
                    self.movie_list("/search/movie", params).await
                }
                MediaType::Tv => {
                    if let Some(year) = year {
                        params.push(("first_air_date_year", year.to_string()));
                    }
                    self.tv_list("/search/tv", params).await
                }
            }
        })
    }

    fn find_by_external_id<'a>(
        &'a self,
        external_id: &'a str,
        source: FindSource,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        Box::pin(async move {
            let found: FindResponse = self
                .get(
                    &format!("/find/{external_id}"),
                    vec![("external_source", source.as_str().to_owned())],
                )
                .await?;
            Ok(found
                .movie_results
                .into_iter()
                .map(normalize_movie)
                .chain(found.tv_results.into_iter().map(normalize_tv))
                .collect())
        })
    }

    fn recommendations_for(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>> {
        Box::pin(async move {
            match media_type {
                MediaType::Movie => {
                    self.movie_list(&format!("/movie/{tmdb_id}/recommendations"), Vec::new())
                        .await
                }
                MediaType::Tv => {
                    self.tv_list(&format!("/tv/{tmdb_id}/recommendations"), Vec::new())
                        .await
                }
            }
        })
    }

    fn discover<'a>(
        &'a self,
        media_type: MediaType,
        options: &'a DiscoverOptions,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        Box::pin(async move {
            let mut params = vec![
                ("include_adult", "false".to_owned()),
                ("sort_by", "vote_average.desc".to_owned()),
                (
                    "vote_count.gte",
                    options.min_vote_count.unwrap_or(300).to_string(),
                ),
                ("page", options.page.unwrap_or(1).to_string()),
            ];
            if let Some(genres) = options.with_genres.as_ref().filter(|g| !g.is_empty()) {
                params.push(("with_genres", join_ids(genres)));
            }
            if let Some(genres) = options.without_genres.as_ref().filter(|g| !g.is_empty()) {
                params.push(("without_genres", join_ids(genres)));
            }
            if let Some(language) = options
                .with_original_language
                .as_ref()
                .filter(|l| !l.is_empty())
            {
                params.push(("with_original_language", language.clone()));
            }
            match media_type {
                MediaType::Movie => self.movie_list("/discover/movie", params).await,
                MediaType::Tv => self.tv_list("/discover/tv", params).await,
            }
        })
    }

    fn trending(&self) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>> {
        Box::pin(async move {
            let list: TrendingList = self.get("/trending/all/week", Vec::new()).await?;
            Ok(list
                .results
                .into_iter()
                .filter_map(|result| match result {
                    TrendingResult::Movie(movie) if !movie.adult => Some(normalize_movie(movie)),
                    TrendingResult::Tv(tv) if !tv.adult => Some(normalize_tv(tv)),
                    _ => None,
                })
                .collect())
        })
    }

    fn title_genre_ids(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<i64>>> {
        Box::pin(async move {
            let details: Details = self
                .get(&format!("/{}/{tmdb_id}", media_type.as_str()), Vec::new())
                .await?;
            Ok(details.genres.iter().map(|g| g.id).collect())
        })
    }

    fn title_details(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<TmdbTitleDetails>> {
        Box::pin(async move {
            match media_type {
                MediaType::Movie => {
                    let details: MovieDetails = self
                        .get(
                            &format!("/movie/{tmdb_id}"),
                            vec![(
                                "append_to_response",
                                "credits,keywords,release_dates".to_owned(),
                            )],
                        )
                        .await?;
                    Ok(normalize_movie_details(details))
                }
                MediaType::Tv => {
                    let details: TvDetails = self
                        .get(
                            &format!("/tv/{tmdb_id}"),
                            vec![(
                                "append_to_response",
                                "credits,keywords,content_ratings".to_owned(),
                            )],
                        )
                        .await?;
                    Ok(normalize_tv_details(details))
                }
            }
        })
    }

    fn title(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<TmdbTitle>> {
        Box::pin(async move {
            let path = format!("/{}/{tmdb_id}", media_type.as_str());
            let title = match media_type {
                MediaType::Movie => {
                    let movie: MovieResult = self.get(&path, Vec::new()).await?;
                    (!movie.adult).then(|| normalize_movie(movie))
                }
                MediaType::Tv => {
                    let tv: TvResult = self.get(&path, Vec::new()).await?;
                    (!tv.adult).then(|| normalize_tv(tv))
                }
            };
            title.ok_or_else(|| {
                IntegrationError::new(format!("TMDB GET {path}"), "adult title excluded")
            })
        })
    }

    fn genre_map(&self, media_type: MediaType) -> BoxFuture<'_, CatalogResult<GenreMap>> {
        Box::pin(async move {
            if let Some(cached) = self
                .genre_cache
                .lock()
                .ok()
                .and_then(|cache| cache.get(&media_type).cloned())
            {
                return Ok(cached);
            }
            let path = match media_type {
                MediaType::Movie => "/genre/movie/list",
                MediaType::Tv => "/genre/tv/list",
            };
            let list: GenreList = self.get(path, Vec::new()).await?;
            let map: GenreMap = Arc::new(list.genres.into_iter().map(|g| (g.id, g.name)).collect());
            if let Ok(mut cache) = self.genre_cache.lock() {
                cache.insert(media_type, map.clone());
            }
            Ok(map)
        })
    }
}
