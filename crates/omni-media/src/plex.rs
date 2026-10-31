//! Plex history, continue-watching and library index.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{StreamExt, TryStreamExt};
use indexmap::IndexMap;
use omni_http::{HttpClient, Method, Url};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::error::IntegrationError;
use crate::js::present;
use crate::types::{ExternalIds, InProgressItem, MediaItem, MediaType, WatchedItem};

const PLEX_TIMEOUT: Duration = Duration::from_secs(15);
/// Retries per Plex request.
const PLEX_RETRIES: u32 = 2;
/// Plex pages are bounded (100 history rows, 500 library rows per request).
const PLEX_MAX_BYTES: usize = 32 * 1024 * 1024;
const DETAIL_CONCURRENCY: usize = 6;
const SECTION_CONCURRENCY: usize = 4;

/// Query parameters in insertion order.
pub type PlexParams = Vec<(String, String)>;

/// `PlexGet`: one JSON GET against the server; errors are messages.
pub trait PlexGet: Send + Sync {
    fn get<'a>(&'a self, path: &'a str, params: PlexParams)
    -> BoxFuture<'a, Result<Value, String>>;
}

fn params(pairs: &[(&str, String)]) -> PlexParams {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PlexGuid {
    #[serde(default, deserialize_with = "present")]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PlexDirectory {
    #[serde(default, deserialize_with = "present")]
    key: Option<String>,
    #[serde(default, rename = "type", deserialize_with = "present")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "present")]
    title: Option<String>,
    #[serde(default, rename = "Location")]
    location: Vec<PlexLocation>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PlexLocation {
    #[serde(default)]
    path: Option<String>,
}

/// A music-type library section and its folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MusicSection {
    pub key: String,
    pub locations: Vec<String>,
}

impl MusicSection {
    /// `path` is one of the section's folders or inside one.
    pub fn contains(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        self.locations.iter().any(|location| {
            let location = location.trim_end_matches('/');
            path == location || path.starts_with(&format!("{location}/"))
        })
    }
}

/// One Plex metadata entry; every field is optional but never `null`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlexMetadata {
    #[serde(default, rename = "type", deserialize_with = "present")]
    pub kind: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub rating_key: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub guid: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub year: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub duration: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub view_offset: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub viewed_at: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub last_viewed_at: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub view_count: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub leaf_count: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub viewed_leaf_count: Option<f64>,
    #[serde(default, rename = "accountID", deserialize_with = "present")]
    pub account_id: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub parent_title: Option<String>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "present")]
    pub added_at: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub grandparent_rating_key: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub grandparent_key: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub grandparent_guid: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub grandparent_title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub grandparent_year: Option<f64>,
    #[serde(default, rename = "Guid", deserialize_with = "present")]
    pub guids: Option<Vec<PlexGuid>>,
}

/// A track in a music library and where its files are.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlexTrack {
    pub album_rating_key: Option<String>,
    pub album: Option<String>,
    pub artist: Option<String>,
    pub files: Vec<String>,
}

#[derive(Deserialize)]
struct TrackPage {
    #[serde(rename = "MediaContainer")]
    media_container: TrackContainer,
}

#[derive(Default, Deserialize)]
struct TrackContainer {
    #[serde(default)]
    size: Option<usize>,
    #[serde(default, rename = "totalSize")]
    total_size: Option<usize>,
    #[serde(default, rename = "Metadata")]
    metadata: Vec<TrackMetadata>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackMetadata {
    #[serde(default)]
    parent_rating_key: Option<String>,
    #[serde(default)]
    parent_title: Option<String>,
    #[serde(default)]
    grandparent_title: Option<String>,
    #[serde(default, rename = "Media")]
    media: Vec<TrackMedia>,
}

#[derive(Default, Deserialize)]
struct TrackMedia {
    #[serde(default, rename = "Part")]
    part: Vec<TrackPart>,
}

#[derive(Default, Deserialize)]
struct TrackPart {
    #[serde(default)]
    file: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct PlexContainer {
    #[serde(default, deserialize_with = "present")]
    size: Option<f64>,
    #[serde(default, rename = "totalSize", deserialize_with = "present")]
    total_size: Option<f64>,
    #[serde(default, rename = "Metadata", deserialize_with = "present")]
    metadata: Option<Vec<PlexMetadata>>,
    #[serde(default, rename = "Directory", deserialize_with = "present")]
    directory: Option<Vec<PlexDirectory>>,
}

#[derive(Deserialize)]
struct PlexResponse {
    #[serde(rename = "MediaContainer")]
    media_container: PlexContainer,
}

fn decode_container(operation: &str, value: Value) -> Result<PlexContainer, IntegrationError> {
    serde_json::from_value::<PlexResponse>(value)
        .map(|decoded| decoded.media_container)
        .map_err(|e| IntegrationError::new(format!("decode {operation}"), e))
}

fn fraction(offset: Option<f64>, duration: Option<f64>) -> Option<f64> {
    let offset = offset?;
    let duration = duration.filter(|d| *d > 0.0)?;
    Some((offset / duration).clamp(0.0, 1.0))
}

static TMDB_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)(?:tmdb|themoviedb)(?:://|/)([0-9]+)").ok());
static IMDB_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)imdb(?:://|/)(tt[0-9]+)").ok());
static TVDB_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)(?:tvdb|thetvdb)(?:://|/)([0-9]+)").ok());
static LIBRARY_KEY_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^/library/metadata/([0-9]+)(?:/|$)").ok());

/// The first capture group of `re` in `text`.
fn capture<'t>(re: &LazyLock<Option<Regex>>, text: &'t str) -> Option<&'t str> {
    re.as_ref()?
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

fn is_digits(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit())
}

/// `Number(digits)` for an id capture.
fn js_number_id(digits: &str) -> Option<i64> {
    let value = omni_core::js::string_to_number(digits);
    (value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15).then(
        || {
            #[allow(clippy::cast_possible_truncation)]
            let id = value as i64;
            id
        },
    )
}

/// Modern `Guid` arrays and legacy agent GUIDs; later
/// GUIDs win.
pub fn parse_external_ids(metadata: &PlexMetadata) -> Option<ExternalIds> {
    let mut ids = ExternalIds::default();
    let guids = metadata
        .guid
        .iter()
        .chain(
            metadata
                .guids
                .iter()
                .flatten()
                .filter_map(|g| g.id.as_ref()),
        )
        .filter(|g| !g.is_empty());
    for guid in guids {
        // A capture beyond the safe-integer range cannot be a real id; it
        // never erases an id an earlier GUID supplied.
        if let Some(tmdb) = capture(&TMDB_RE, guid).and_then(js_number_id) {
            ids.tmdb = Some(tmdb);
        }
        if let Some(imdb) = capture(&IMDB_RE, guid) {
            ids.imdb = Some(imdb.to_owned());
        }
        if let Some(tvdb) = capture(&TVDB_RE, guid).and_then(js_number_id) {
            ids.tvdb = Some(tvdb);
        }
    }
    (!ids.is_empty()).then_some(ids)
}

fn native_guid(metadata: &PlexMetadata) -> String {
    match (&metadata.guid, &metadata.rating_key) {
        (Some(guid), _) => guid.clone(),
        (None, Some(key)) if !key.is_empty() => {
            format!(
                "plex://{}/{key}",
                metadata.kind.as_deref().unwrap_or("undefined")
            )
        }
        _ => String::new(),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn js_int(value: Option<f64>) -> Option<i64> {
    value.map(|v| v as i64)
}

fn media_item(metadata: &PlexMetadata, media_type: MediaType) -> Option<MediaItem> {
    let guid = native_guid(metadata);
    let title = metadata.title.clone().filter(|t| !t.is_empty())?;
    if guid.is_empty() {
        return None;
    }
    Some(MediaItem {
        guid,
        title,
        year: js_int(metadata.year),
        media_type,
        external_ids: parse_external_ids(metadata),
        title_slug: None,
    })
}

fn series_key(metadata: &PlexMetadata) -> Option<String> {
    metadata
        .grandparent_rating_key
        .clone()
        .or_else(|| {
            metadata
                .grandparent_key
                .as_deref()
                .and_then(|key| capture(&LIBRARY_KEY_RE, key))
                .map(str::to_owned)
        })
        .or_else(|| metadata.grandparent_guid.clone())
}

fn episode_series(metadata: &PlexMetadata, detail: Option<&PlexMetadata>) -> Option<MediaItem> {
    let title = detail
        .and_then(|d| d.title.clone())
        .or_else(|| metadata.grandparent_title.clone())
        .filter(|t| !t.is_empty())?;
    let guid = match detail {
        Some(detail) => native_guid(detail),
        None => metadata.grandparent_guid.clone().unwrap_or_default(),
    };
    if guid.is_empty() {
        return None;
    }
    Some(MediaItem {
        guid,
        title,
        year: js_int(detail.and_then(|d| d.year).or(metadata.grandparent_year)),
        media_type: MediaType::Tv,
        external_ids: detail.and_then(parse_external_ids),
        title_slug: None,
    })
}

/// Plex timestamps are Unix seconds; recommendation timestamps are epoch ms.
#[allow(clippy::cast_possible_truncation)]
fn timestamp(metadata: &PlexMetadata) -> i64 {
    (metadata
        .viewed_at
        .or(metadata.last_viewed_at)
        .unwrap_or(0.0)
        * 1000.0) as i64
}

fn series_progress(detail: Option<&PlexMetadata>, current_episode_progress: f64) -> Option<f64> {
    let detail = detail?;
    let leaf_count = detail.leaf_count.filter(|c| *c > 0.0)?;
    Some(
        ((detail.viewed_leaf_count.unwrap_or(0.0) + current_episode_progress) / leaf_count)
            .clamp(0.0, 1.0),
    )
}

/// Plex reads behind a [`PlexGet`] transport.
#[derive(Clone)]
pub struct PlexClient {
    get: Arc<dyn PlexGet>,
    account_id: Option<u64>,
}

impl PlexClient {
    pub fn new(get: Arc<dyn PlexGet>, account_id: Option<u64>) -> Self {
        Self { get, account_id }
    }

    async fn fetch(
        &self,
        operation: &str,
        path: &str,
        params: PlexParams,
    ) -> Result<Value, IntegrationError> {
        self.get
            .get(path, params)
            .await
            .map_err(|cause| IntegrationError::new(operation, cause))
    }

    async fn container(
        &self,
        operation: &str,
        path: &str,
        params: PlexParams,
    ) -> Result<PlexContainer, IntegrationError> {
        let raw = self.fetch(operation, path, params).await?;
        decode_container(operation, raw)
    }

    /// Details for each distinct numeric key; lookups that fail are skipped.
    async fn metadata_details(&self, keys: Vec<Option<String>>) -> HashMap<String, PlexMetadata> {
        let mut seen = HashSet::new();
        let unique: Vec<String> = keys
            .into_iter()
            .flatten()
            .filter(|key| !key.is_empty() && seen.insert(key.clone()))
            .collect();
        futures::stream::iter(
            unique
                .into_iter()
                .map(|key| async move {
                    if !is_digits(&key) {
                        return (key, None);
                    }
                    let operation = format!("Plex metadata {key}");
                    let detail = self
                        .container(
                            &operation,
                            &format!("/library/metadata/{key}"),
                            params(&[("includeGuids", "1".to_owned())]),
                        )
                        .await
                        .ok()
                        .and_then(|c| c.metadata.and_then(|m| m.into_iter().next()));
                    (key, detail)
                })
                .collect::<Vec<_>>(),
        )
        .buffered(DETAIL_CONCURRENCY)
        .filter_map(|(key, detail)| async move { detail.map(|d| (key, d)) })
        .collect()
        .await
    }

    async fn show_details(&self, metadata: &[&PlexMetadata]) -> HashMap<String, PlexMetadata> {
        self.metadata_details(metadata.iter().map(|m| series_key(m)).collect())
            .await
    }

    /// Paginates `path` (`X-Plex-Container-Start/Size`) until a short page.
    async fn paginate(
        &self,
        operation: &str,
        path: &str,
        size: usize,
        query: impl Fn(usize) -> PlexParams,
    ) -> Result<Vec<PlexMetadata>, IntegrationError> {
        let mut metadata = Vec::new();
        let mut start = 0usize;
        loop {
            let container = self.container(operation, path, query(start)).await?;
            let page = container.metadata.unwrap_or_default();
            let page_len = page.len();
            metadata.extend(page);
            #[allow(clippy::cast_precision_loss)]
            let total = container
                .total_size
                .or(container.size)
                .unwrap_or(page_len as f64);
            #[allow(clippy::cast_precision_loss)]
            let seen = (start + page_len) as f64;
            if page_len == 0 || seen >= total {
                break;
            }
            start += size;
        }
        Ok(metadata)
    }

    /// Completed and partial watches: movies individually, episodes
    /// aggregated per series (series progress from leaf counts).
    pub async fn fetch_watch_history(&self) -> Result<Vec<WatchedItem>, IntegrationError> {
        const SIZE: usize = 100;
        let account = self.account_id.filter(|id| *id != 0);
        let metadata = self
            .paginate(
                "Plex watch history",
                "/status/sessions/history/all",
                SIZE,
                |start| {
                    let mut query = params(&[
                        ("sort", "viewedAt:desc".to_owned()),
                        ("X-Plex-Container-Start", start.to_string()),
                        ("X-Plex-Container-Size", SIZE.to_string()),
                        ("includeGuids", "1".to_owned()),
                    ]);
                    if let Some(account) = account {
                        query.push(("accountID".to_owned(), account.to_string()));
                    }
                    query
                },
            )
            .await?;

        let account_ids: HashSet<i64> = metadata
            .iter()
            .filter_map(|m| m.account_id.filter(|id| id.fract() == 0.0))
            .map(|id| {
                #[allow(clippy::cast_possible_truncation)]
                let id = id as i64;
                id
            })
            .collect();
        if account.is_none() && account_ids.len() > 1 {
            return Err(IntegrationError::new(
                "validate Plex watch history account scope",
                "Plex history contains multiple accounts; configure PLEX_ACCOUNT_ID",
            ));
        }

        let episodes: Vec<&PlexMetadata> = metadata
            .iter()
            .filter(|m| m.kind.as_deref() == Some("episode"))
            .collect();
        let movie_keys: Vec<Option<String>> = metadata
            .iter()
            .filter(|m| m.kind.as_deref() == Some("movie"))
            .map(|m| m.rating_key.clone())
            .collect();
        let (details, movie_details) = futures::join!(
            self.show_details(&episodes),
            self.metadata_details(movie_keys)
        );

        let mut watched = Vec::new();
        let mut shows: IndexMap<String, (WatchedItem, String)> = IndexMap::new();
        for entry in &metadata {
            match entry.kind.as_deref() {
                Some("movie") => {
                    let detail = movie_details.get(entry.rating_key.as_deref().unwrap_or(""));
                    let Some(item) = media_item(detail.unwrap_or(entry), MediaType::Movie) else {
                        continue;
                    };
                    #[allow(clippy::cast_possible_truncation)]
                    let view_count = entry.view_count.unwrap_or(1.0) as i64;
                    watched.push(WatchedItem {
                        item,
                        viewed_at: timestamp(entry),
                        view_count,
                        completion: fraction(entry.view_offset, entry.duration),
                    });
                }
                Some("episode") => {
                    // `if (!key) continue`: an empty key is no series.
                    let Some(key) = series_key(entry).filter(|key| !key.is_empty()) else {
                        continue;
                    };
                    let Some(item) = episode_series(entry, details.get(&key)) else {
                        continue;
                    };
                    match shows.get_mut(&item.guid) {
                        Some((existing, _)) => {
                            // An episode replay is not a series replay: viewCount stays 1.
                            existing.viewed_at = existing.viewed_at.max(timestamp(entry));
                        }
                        None => {
                            shows.insert(
                                item.guid.clone(),
                                (
                                    WatchedItem {
                                        item,
                                        viewed_at: timestamp(entry),
                                        view_count: 1,
                                        completion: None,
                                    },
                                    key,
                                ),
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        for (_, (mut item, detail_key)) in shows {
            item.completion = series_progress(details.get(&detail_key), 0.0);
            watched.push(item);
        }
        Ok(watched)
    }

    /// Partially watched movies and series from Continue Watching.
    pub async fn fetch_in_progress(&self) -> Result<Vec<InProgressItem>, IntegrationError> {
        let container = self
            .container(
                "Plex continue watching",
                "/hubs/home/continueWatching",
                params(&[("includeGuids", "1".to_owned())]),
            )
            .await?;
        let metadata = container.metadata.unwrap_or_default();
        let episodes: Vec<&PlexMetadata> = metadata
            .iter()
            .filter(|m| m.kind.as_deref() == Some("episode"))
            .collect();
        let details = self.show_details(&episodes).await;
        let mut items: IndexMap<String, InProgressItem> = IndexMap::new();
        for entry in &metadata {
            let Some(episode_progress) = fraction(entry.view_offset, entry.duration) else {
                continue;
            };
            if episode_progress <= 0.0 || episode_progress >= 1.0 {
                continue;
            }
            let is_episode = entry.kind.as_deref() == Some("episode");
            let detail = if is_episode {
                details.get(&series_key(entry).unwrap_or_default())
            } else {
                None
            };
            let progress = if is_episode {
                series_progress(detail, episode_progress).unwrap_or(episode_progress)
            } else {
                episode_progress
            };
            let item = match entry.kind.as_deref() {
                Some("movie") => media_item(entry, MediaType::Movie),
                Some("episode") => episode_series(entry, detail),
                _ => None,
            };
            let Some(item) = item else {
                continue;
            };
            let last_viewed_at = timestamp(entry);
            let replace = items
                .get(&item.guid)
                .is_none_or(|prior| last_viewed_at >= prior.last_viewed_at);
            if replace {
                items.insert(
                    item.guid.clone(),
                    InProgressItem {
                        item,
                        progress,
                        last_viewed_at,
                    },
                );
            }
        }
        Ok(items.into_values().collect())
    }

    /// The music-type library section titled `title`.
    pub async fn music_section(
        &self,
        title: &str,
    ) -> Result<Option<MusicSection>, IntegrationError> {
        let sections = self
            .container("Plex library sections", "/library/sections", Vec::new())
            .await?;
        Ok(sections
            .directory
            .unwrap_or_default()
            .into_iter()
            .find(|s| s.kind.as_deref() == Some("artist") && s.title.as_deref() == Some(title))
            .and_then(|s| {
                Some(MusicSection {
                    key: s.key.filter(|k| !k.is_empty())?,
                    locations: s.location.into_iter().filter_map(|l| l.path).collect(),
                })
            }))
    }

    /// Every album in the music-type library section titled `title`, or
    /// `None` when there is no such section.
    pub async fn fetch_music_albums(
        &self,
        title: &str,
    ) -> Result<Option<Vec<PlexMetadata>>, IntegrationError> {
        let Some(MusicSection { key, .. }) = self.music_section(title).await? else {
            return Ok(None);
        };
        const SIZE: usize = 500;
        /// Plex's album metadata type.
        const ALBUM: &str = "9";
        let albums = self
            .paginate(
                &format!("Plex library section {key}"),
                &format!("/library/sections/{key}/all"),
                SIZE,
                |start| {
                    params(&[
                        ("type", ALBUM.to_owned()),
                        ("X-Plex-Container-Start", start.to_string()),
                        ("X-Plex-Container-Size", SIZE.to_string()),
                    ])
                },
            )
            .await?;
        Ok(Some(albums))
    }

    /// Every track in music section `key` with its album and files.
    pub async fn fetch_music_tracks(&self, key: &str) -> Result<Vec<PlexTrack>, IntegrationError> {
        const SIZE: usize = 500;
        /// Plex's track metadata type.
        const TRACK: &str = "10";
        let operation = format!("Plex library section {key} tracks");
        let mut tracks = Vec::new();
        let mut start = 0usize;
        loop {
            let raw = self
                .fetch(
                    &operation,
                    &format!("/library/sections/{key}/all"),
                    params(&[
                        ("type", TRACK.to_owned()),
                        ("X-Plex-Container-Start", start.to_string()),
                        ("X-Plex-Container-Size", SIZE.to_string()),
                    ]),
                )
                .await?;
            let page: TrackPage = serde_json::from_value(raw)
                .map_err(|e| IntegrationError::new(format!("decode {operation}"), e))?;
            let container = page.media_container;
            let count = container.metadata.len();
            tracks.extend(container.metadata.into_iter().map(|t| {
                PlexTrack {
                    album_rating_key: t.parent_rating_key,
                    album: t.parent_title,
                    artist: t.grandparent_title,
                    files: t
                        .media
                        .into_iter()
                        .flat_map(|m| m.part.into_iter().filter_map(|p| p.file))
                        .collect(),
                }
            }));
            let total = container.total_size.or(container.size).unwrap_or(0);
            start += count;
            if count == 0 || start >= total {
                break;
            }
        }
        Ok(tracks)
    }

    /// Every movie and show in the movie/show library sections.
    pub async fn fetch_library_index(&self) -> Result<Vec<MediaItem>, IntegrationError> {
        let sections = self
            .container("Plex library sections", "/library/sections", Vec::new())
            .await?;
        let keys: Vec<String> = sections
            .directory
            .unwrap_or_default()
            .into_iter()
            .filter(|s| matches!(s.kind.as_deref(), Some("movie" | "show")))
            .filter_map(|s| s.key.filter(|k| !k.is_empty()))
            .collect();
        let containers: Vec<Vec<PlexMetadata>> = futures::stream::iter(
            keys.into_iter()
                .map(|key| async move {
                    const SIZE: usize = 500;
                    self.paginate(
                        &format!("Plex library section {key}"),
                        &format!("/library/sections/{key}/all"),
                        SIZE,
                        |start| {
                            params(&[
                                ("includeGuids", "1".to_owned()),
                                ("X-Plex-Container-Start", start.to_string()),
                                ("X-Plex-Container-Size", SIZE.to_string()),
                            ])
                        },
                    )
                    .await
                })
                .collect::<Vec<_>>(),
        )
        .buffered(SECTION_CONCURRENCY)
        .try_collect()
        .await?;
        Ok(containers
            .iter()
            .flatten()
            .filter_map(|entry| {
                let media_type = match entry.kind.as_deref() {
                    Some("movie") => MediaType::Movie,
                    Some("show") => MediaType::Tv,
                    _ => return None,
                };
                media_item(entry, media_type)
            })
            .collect())
    }
}

/// The production [`PlexGet`]: JSON GETs with the Plex token, a 15 s timeout
/// and two retries for transient failures.
pub struct HttpPlexGet {
    http: HttpClient,
    base_url: String,
    token: String,
}

impl HttpPlexGet {
    pub fn new(http: HttpClient, base_url: &str, token: String) -> Self {
        Self {
            http,
            base_url: base_url.strip_suffix('/').unwrap_or(base_url).to_owned(),
            token,
        }
    }

    async fn get_once(&self, url: &Url) -> Result<Value, omni_http::HttpError> {
        self.http
            .request(Method::GET, url.clone())
            .header("Accept", "application/json")
            .header("X-Plex-Token", self.token.as_str())
            .timeout(PLEX_TIMEOUT)
            .json_bounded::<Value>(PLEX_MAX_BYTES)
            .await
    }
}

/// Timeouts, network failures and transient statuses.
fn transient(error: &omni_http::HttpError) -> bool {
    match error {
        omni_http::HttpError::Timeout | omni_http::HttpError::Network(_) => true,
        omni_http::HttpError::Status { status, .. } => {
            matches!(
                status,
                408 | 413 | 429 | 500 | 502 | 503 | 504 | 521 | 522 | 524
            )
        }
        _ => false,
    }
}

impl PlexGet for HttpPlexGet {
    fn get<'a>(
        &'a self,
        path: &'a str,
        params: PlexParams,
    ) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let mut url = Url::parse(&format!("{}{path}", self.base_url))
                .map_err(|e| format!("Invalid URL: {e}"))?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(params.iter());
            }
            let mut attempt = 0u32;
            loop {
                match self.get_once(&url).await {
                    Ok(value) => return Ok(value),
                    Err(error) if attempt < PLEX_RETRIES && transient(&error) => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(1000 * (1 << (attempt - 1))))
                            .await;
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        })
    }
}
