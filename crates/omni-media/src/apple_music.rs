//! Apple Music link and id resolution through Apple's keyless iTunes Lookup
//! API (`https://itunes.apple.com/lookup`). A caller's link is parsed locally
//! and never fetched: the only request goes to the fixed lookup endpoint, so
//! no caller input chooses a host.

use std::time::Duration;

use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method, Url};
use serde::{Deserialize, Serialize};

const LOOKUP_URL: &str = "https://itunes.apple.com/lookup";
const DEFAULT_STOREFRONT: &str = "ca";
/// A single lookup is about 2 KB; the bound only guards a misbehaving upstream.
const RESPONSE_MAX_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REFERENCE_CHARS: usize = 2048;
/// Apple catalog ids are at most 10 digits today; this leaves headroom
/// without accepting numbers that cannot be ids.
const MAX_ID_DIGITS: usize = 15;

/// Hosts whose links name Apple Music catalog items.
const MUSIC_HOSTS: [&str; 7] = [
    "geo.itunes.apple.com",
    "music.apple.com",
    "geo.music.apple.com",
    "embed.music.apple.com",
    "beta.music.apple.com",
    "classical.music.apple.com",
    "itunes.apple.com",
];

/// The catalog item kinds the public lookup resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicType {
    Song,
    Album,
    Artist,
    MusicVideo,
}

impl MusicType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::MusicVideo => "music video",
        }
    }

    fn with_article(self) -> &'static str {
        match self {
            Self::Song => "a song",
            Self::Album => "an album",
            Self::Artist => "an artist",
            Self::MusicVideo => "a music video",
        }
    }
}

/// A catalog id taken from a link or a bare id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRef {
    pub id: u64,
    /// The kind the link names; `None` for a bare id.
    pub kind: Option<MusicType>,
    pub storefront: Option<String>,
    /// The album of an `album/...?i=<song>` link, whose `id` is the song.
    pub album_id: Option<u64>,
}

/// What a reference names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Catalog(CatalogRef),
    /// A recognizable Apple link the public lookup cannot resolve.
    Unsupported {
        kind: String,
        reason: String,
        storefront: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    #[error("reference is empty")]
    Empty,
    #[error("reference is longer than {MAX_REFERENCE_CHARS} characters")]
    TooLong,
    #[error("not an Apple Music link or numeric catalog id")]
    NotApple,
    #[error("Apple Music link has no catalog id: {0}")]
    Malformed(String),
    #[error("storefront must be a two-letter country code, got {0:?}")]
    Storefront(String),
    #[error("the link names {link}, but type {requested} was requested")]
    TypeConflict {
        link: &'static str,
        requested: &'static str,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AppleMusicError {
    #[error(transparent)]
    Reference(#[from] ReferenceError),
    #[error("Apple does not recognize storefront {storefront:?} (from the {source_label}){hint}")]
    UnknownStorefront {
        storefront: String,
        source_label: &'static str,
        hint: &'static str,
    },
    #[error("Apple catalog lookup failed: {0}")]
    Provider(String),
}

impl AppleMusicError {
    /// Caller mistakes, as opposed to upstream failures.
    pub fn is_input(&self) -> bool {
        matches!(self, Self::Reference(_) | Self::UnknownStorefront { .. })
    }
}

/// Echoed labels come from the caller's path or Apple's response; keep them short.
const MAX_LABEL_CHARS: usize = 64;

fn bounded_label(label: &str) -> String {
    label.chars().take(MAX_LABEL_CHARS).collect()
}

/// Whether `input` starts with its own `scheme://`, as opposed to a `://`
/// later in a scheme-less link's query.
fn has_scheme(input: &str) -> bool {
    input.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// Lowercased two-letter storefront, or an error.
pub fn normalize_storefront(value: &str) -> Result<String, ReferenceError> {
    let trimmed = value.trim();
    if trimmed.len() == 2 && trimmed.bytes().all(|b| b.is_ascii_alphabetic()) {
        Ok(trimmed.to_ascii_lowercase())
    } else {
        Err(ReferenceError::Storefront(trimmed.to_owned()))
    }
}

fn parse_id(segment: &str) -> Option<u64> {
    let digits = segment.strip_prefix("id").unwrap_or(segment);
    if digits.is_empty()
        || digits.len() > MAX_ID_DIGITS
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok().filter(|id| *id != 0)
}

fn unsupported(kind: &str, storefront: Option<String>) -> Parsed {
    let reason = match kind {
        "playlist" => {
            "Apple's public catalog lookup does not resolve playlists; they need the authenticated Apple Music API"
        }
        "library" => "links into a personal Apple Music library are private",
        "short_link" => {
            "apple.co short links are not followed; open the link and share the music.apple.com address it leads to"
        }
        _ => "Apple's public catalog lookup resolves only songs, albums, artists and music videos",
    };
    Parsed::Unsupported {
        kind: bounded_label(kind),
        reason: reason.to_owned(),
        storefront,
    }
}

/// Parses a bare catalog id or an Apple Music link without fetching it.
pub fn parse_reference(input: &str) -> Result<Parsed, ReferenceError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ReferenceError::Empty);
    }
    if input.chars().count() > MAX_REFERENCE_CHARS {
        return Err(ReferenceError::TooLong);
    }
    if input.bytes().all(|b| b.is_ascii_digit()) {
        let id = parse_id(input).ok_or(ReferenceError::NotApple)?;
        return Ok(Parsed::Catalog(CatalogRef {
            id,
            kind: None,
            storefront: None,
            album_id: None,
        }));
    }
    let with_scheme;
    let text = if has_scheme(input) {
        input
    } else {
        with_scheme = format!("https://{input}");
        &with_scheme
    };
    let url = Url::parse(text).map_err(|_| ReferenceError::NotApple)?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(ReferenceError::NotApple);
    }
    let host = url.host_str().unwrap_or_default();
    if host == "apple.co" {
        return Ok(unsupported("short_link", None));
    }
    if !MUSIC_HOSTS.contains(&host) {
        return Err(ReferenceError::NotApple);
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let mut rest = segments.as_slice();
    let mut storefront = None;
    if let Some((first, tail)) = rest.split_first()
        && first.len() == 2
        && first.bytes().all(|b| b.is_ascii_alphabetic())
    {
        storefront = Some(first.to_ascii_lowercase());
        rest = tail;
    }
    let Some((kind_segment, tail)) = rest.split_first() else {
        return Err(ReferenceError::Malformed(
            "no item type in the path".to_owned(),
        ));
    };
    let kind = match *kind_segment {
        "album" => MusicType::Album,
        "song" => MusicType::Song,
        "artist" => MusicType::Artist,
        "music-video" => MusicType::MusicVideo,
        other => return Ok(unsupported(other, storefront)),
    };
    let id = tail
        .last()
        .and_then(|segment| parse_id(segment))
        .ok_or_else(|| ReferenceError::Malformed(format!("no numeric {} id", kind.as_str())))?;
    if kind == MusicType::Album
        && let Some((_, song)) = url.query_pairs().find(|(key, _)| key == "i")
    {
        let song = parse_id(&song).ok_or_else(|| {
            ReferenceError::Malformed("the ?i= song id is not numeric".to_owned())
        })?;
        return Ok(Parsed::Catalog(CatalogRef {
            id: song,
            kind: Some(MusicType::Song),
            storefront,
            album_id: Some(id),
        }));
    }
    Ok(Parsed::Catalog(CatalogRef {
        id,
        kind: Some(kind),
        storefront,
        album_id: None,
    }))
}

/// Where the storefront used for the lookup came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorefrontSource {
    Input,
    Link,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStatus {
    Found,
    NotFound,
    TypeMismatch,
    Unsupported,
}

/// One resolved catalog item.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicItem {
    #[serde(rename = "type")]
    pub kind: MusicType,
    pub id: u64,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streamable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
}

/// The tool's answer for one reference.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub status: ResolveStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<MusicType>,
    pub storefront: String,
    pub storefront_source: StorefrontSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<MusicItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ResolveRequest {
    pub reference: String,
    pub kind: Option<MusicType>,
    pub storefront: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawResult {
    wrapper_type: Option<String>,
    kind: Option<String>,
    collection_type: Option<String>,
    artist_type: Option<String>,
    track_id: Option<u64>,
    collection_id: Option<u64>,
    artist_id: Option<u64>,
    track_name: Option<String>,
    collection_name: Option<String>,
    artist_name: Option<String>,
    track_view_url: Option<String>,
    collection_view_url: Option<String>,
    artist_view_url: Option<String>,
    artist_link_url: Option<String>,
    artwork_url100: Option<String>,
    release_date: Option<String>,
    primary_genre_name: Option<String>,
    track_explicitness: Option<String>,
    collection_explicitness: Option<String>,
    track_number: Option<u64>,
    disc_number: Option<u64>,
    track_count: Option<u64>,
    track_time_millis: Option<u64>,
    is_streamable: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct LookupResponse {
    #[serde(default)]
    results: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LookupErrorBody {
    error_message: Option<String>,
}

/// The kind of a lookup result, or the provider's own label for anything else.
fn classify(raw: &RawResult) -> Result<MusicType, String> {
    let label = |value: Option<&str>, fallback: &str| {
        bounded_label(&value.unwrap_or(fallback).to_ascii_lowercase())
    };
    match raw.wrapper_type.as_deref().unwrap_or_default() {
        "track" => match raw.kind.as_deref() {
            Some("song") => Ok(MusicType::Song),
            Some("music-video") => Ok(MusicType::MusicVideo),
            other => Err(label(other, "track")),
        },
        "collection" => match raw.collection_type.as_deref() {
            Some("Album" | "Compilation") => Ok(MusicType::Album),
            other => Err(label(other, "collection")),
        },
        "artist" => match raw.artist_type.as_deref() {
            Some("Artist") => Ok(MusicType::Artist),
            other => Err(label(other, "artist")),
        },
        "" => Err(label(raw.kind.as_deref(), "unknown")),
        other => Err(label(Some(other), "unknown")),
    }
}

/// The result's own id, whatever its kind (to match it to the request).
fn raw_id(raw: &RawResult) -> Option<u64> {
    match raw.wrapper_type.as_deref() {
        Some("artist") => raw.artist_id,
        Some("collection") => raw.collection_id,
        Some("track") => raw.track_id,
        _ => raw.track_id.or(raw.collection_id),
    }
}

/// An Apple link with tracking parameters (`uo`, `app`, `at`, `ct`) removed;
/// `drop_song` also removes `?i=` to name the album itself. Non-Apple links
/// are dropped rather than passed through.
fn clean_link(link: Option<&str>, drop_song: bool) -> Option<String> {
    let mut url = Url::parse(link?).ok()?;
    if url.scheme() != "https" || !MUSIC_HOSTS.contains(&url.host_str()?) {
        return None;
    }
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key == "i" && !drop_song)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    url.set_fragment(None);
    Some(url.into())
}

fn artwork_link(link: Option<&str>) -> Option<String> {
    let url = Url::parse(link?).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https" && host.ends_with(".mzstatic.com")).then(|| url.into())
}

fn explicitness(value: Option<&str>) -> Option<bool> {
    match value? {
        "explicit" => Some(true),
        "cleaned" | "notExplicit" => Some(false),
        _ => None,
    }
}

fn release_day(value: Option<&str>) -> Option<String> {
    let value = value?;
    let day = value.get(..10)?;
    let bytes = day.as_bytes();
    let shaped = bytes.iter().enumerate().all(|(index, b)| {
        if index == 4 || index == 7 {
            *b == b'-'
        } else {
            b.is_ascii_digit()
        }
    });
    shaped.then(|| day.to_owned())
}

fn item_from(raw: RawResult, kind: MusicType, id: u64) -> Option<MusicItem> {
    let non_empty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let artist_url = clean_link(
        raw.artist_view_url
            .as_deref()
            .or(raw.artist_link_url.as_deref()),
        true,
    );
    let mut item = MusicItem {
        kind,
        id,
        name: String::new(),
        artist_name: None,
        artist_id: None,
        album_name: None,
        album_id: None,
        track_number: None,
        disc_number: None,
        track_count: None,
        duration_ms: None,
        release_date: release_day(raw.release_date.as_deref()),
        genre: non_empty(raw.primary_genre_name),
        explicit: None,
        streamable: raw.is_streamable,
        url: None,
        artist_url: None,
        album_url: None,
        artwork_url: artwork_link(raw.artwork_url100.as_deref()),
    };
    match kind {
        MusicType::Song | MusicType::MusicVideo => {
            item.name = non_empty(raw.track_name)?;
            item.artist_name = non_empty(raw.artist_name);
            item.artist_id = raw.artist_id;
            item.album_name = non_empty(raw.collection_name);
            item.album_id = raw.collection_id;
            item.track_number = raw.track_number;
            item.disc_number = raw.disc_number;
            item.track_count = raw.track_count;
            item.duration_ms = raw.track_time_millis;
            item.explicit = explicitness(raw.track_explicitness.as_deref());
            item.url = clean_link(raw.track_view_url.as_deref(), false);
            item.artist_url = artist_url;
            item.album_url = raw
                .collection_id
                .and_then(|_| clean_link(raw.collection_view_url.as_deref(), true));
        }
        MusicType::Album => {
            item.name = non_empty(raw.collection_name)?;
            item.artist_name = non_empty(raw.artist_name);
            item.artist_id = raw.artist_id;
            item.track_count = raw.track_count;
            item.explicit = explicitness(raw.collection_explicitness.as_deref());
            item.url = clean_link(raw.collection_view_url.as_deref(), true);
            item.artist_url = artist_url;
        }
        MusicType::Artist => {
            item.name = non_empty(raw.artist_name)?;
            item.url = artist_url;
            item.release_date = None;
        }
    }
    Some(item)
}

/// What one lookup returned for the requested id.
#[derive(Debug, PartialEq)]
pub enum LookupOutcome {
    Found(Box<MusicItem>),
    /// The id exists but is not music (an app, podcast, book, ...).
    NotMusic(String),
    NotFound,
}

/// Decodes a lookup body for `id`. A result for a different id is ignored.
pub fn parse_lookup(id: u64, body: &str) -> Result<LookupOutcome, AppleMusicError> {
    let response: LookupResponse = serde_json::from_str(body)
        .map_err(|e| AppleMusicError::Provider(format!("unreadable lookup response: {e}")))?;
    for value in response.results {
        let raw: RawResult = serde_json::from_value(value)
            .map_err(|e| AppleMusicError::Provider(format!("unreadable lookup result: {e}")))?;
        if raw_id(&raw) != Some(id) {
            continue;
        }
        return match classify(&raw) {
            Ok(kind) => item_from(raw, kind, id)
                .map(|item| LookupOutcome::Found(Box::new(item)))
                .ok_or_else(|| {
                    AppleMusicError::Provider(format!("lookup result for {id} has no name"))
                }),
            Err(other) => Ok(LookupOutcome::NotMusic(other)),
        };
    }
    Ok(LookupOutcome::NotFound)
}

/// Apple Music catalog lookups over the SSRF-guarded public client.
#[derive(Clone)]
pub struct AppleMusic {
    http: PublicHttpClient,
}

impl AppleMusic {
    pub fn new(http: PublicHttpClient) -> Self {
        Self { http }
    }

    /// One `GET /lookup?id=<id>&country=<storefront>`.
    async fn lookup(
        &self,
        id: u64,
        storefront: &str,
        source: StorefrontSource,
    ) -> Result<LookupOutcome, AppleMusicError> {
        let url = Url::parse(LOOKUP_URL).map_err(|e| AppleMusicError::Provider(e.to_string()))?;
        let id_text = id.to_string();
        let response = self
            .http
            .request(Method::GET, url)
            .query(&[("id", id_text.as_str()), ("country", storefront)])
            .timeout(REQUEST_TIMEOUT)
            .send_bounded(RESPONSE_MAX_BYTES)
            .await
            .map_err(|e| AppleMusicError::Provider(provider_detail(&e)))?;
        let body = String::from_utf8_lossy(&response.body);
        if response.status.as_u16() == 400 {
            let message = serde_json::from_str::<LookupErrorBody>(&body)
                .ok()
                .and_then(|b| b.error_message)
                .unwrap_or_default();
            if message.contains("[country]") {
                return Err(AppleMusicError::UnknownStorefront {
                    storefront: storefront.to_owned(),
                    source_label: match source {
                        StorefrontSource::Input => "storefront input",
                        StorefrontSource::Link => "link",
                        StorefrontSource::Default => "default",
                    },
                    hint: match source {
                        StorefrontSource::Link => "; pass a storefront to override the link's",
                        _ => "",
                    },
                });
            }
            return Err(AppleMusicError::Provider(format!(
                "HTTP 400{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", message.chars().take(200).collect::<String>())
                }
            )));
        }
        if matches!(response.status.as_u16(), 403 | 429) {
            return Err(AppleMusicError::Provider(format!(
                "HTTP {}: Apple is rate limiting lookups; retry shortly",
                response.status.as_u16()
            )));
        }
        if !response.status.is_success() {
            return Err(AppleMusicError::Provider(format!(
                "HTTP {}",
                response.status.as_u16()
            )));
        }
        parse_lookup(id, &body)
    }

    /// Resolves a link or id. Unsupported links answer without a request.
    pub async fn resolve(&self, request: ResolveRequest) -> Result<Resolution, AppleMusicError> {
        let input_storefront = request
            .storefront
            .as_deref()
            .map(normalize_storefront)
            .transpose()?;
        let parsed = parse_reference(&request.reference)?;
        let catalog = match parsed {
            Parsed::Catalog(catalog) => catalog,
            Parsed::Unsupported {
                kind,
                reason,
                storefront,
            } => {
                let (storefront, source) = pick_storefront(input_storefront, storefront);
                return Ok(Resolution {
                    status: ResolveStatus::Unsupported,
                    id: None,
                    expected_type: None,
                    storefront,
                    storefront_source: source,
                    item: None,
                    unsupported_kind: Some(kind),
                    message: Some(reason),
                });
            }
        };
        let (storefront, source) = pick_storefront(input_storefront, catalog.storefront.clone());
        let (id, expected) = target(&catalog, request.kind)?;
        let outcome = self.lookup(id, &storefront, source).await?;
        let base = Resolution {
            status: ResolveStatus::NotFound,
            id: Some(id),
            expected_type: expected,
            storefront: storefront.clone(),
            storefront_source: source,
            item: None,
            unsupported_kind: None,
            message: None,
        };
        Ok(match outcome {
            LookupOutcome::Found(item) => match expected {
                Some(want) if want != item.kind => Resolution {
                    status: ResolveStatus::TypeMismatch,
                    message: Some(format!(
                        "id {id} is {}, not {}",
                        item.kind.with_article(),
                        want.with_article()
                    )),
                    item: Some(*item),
                    ..base
                },
                _ => Resolution {
                    status: ResolveStatus::Found,
                    item: Some(*item),
                    ..base
                },
            },
            LookupOutcome::NotMusic(kind) => Resolution {
                status: ResolveStatus::Unsupported,
                message: Some(format!(
                    "id {id} is an Apple catalog item of kind {kind:?}, not a song, album, artist or music video"
                )),
                unsupported_kind: Some(kind),
                ..base
            },
            LookupOutcome::NotFound => Resolution {
                message: Some(not_found_message(id, &storefront, source)),
                ..base
            },
        })
    }
}

/// The id to look up and the kind it must be. An explicit `album` type on an
/// `album/...?i=<song>` link selects the album instead of the song.
fn target(
    catalog: &CatalogRef,
    requested: Option<MusicType>,
) -> Result<(u64, Option<MusicType>), ReferenceError> {
    match (catalog.kind, requested) {
        (Some(MusicType::Song), Some(MusicType::Album)) if catalog.album_id.is_some() => Ok((
            catalog.album_id.unwrap_or(catalog.id),
            Some(MusicType::Album),
        )),
        (Some(link), Some(want)) if link != want => Err(ReferenceError::TypeConflict {
            link: link.as_str(),
            requested: want.as_str(),
        }),
        (link, want) => Ok((catalog.id, want.or(link))),
    }
}

fn pick_storefront(input: Option<String>, link: Option<String>) -> (String, StorefrontSource) {
    match (input, link) {
        (Some(input), _) => (input, StorefrontSource::Input),
        (None, Some(link)) => (link, StorefrontSource::Link),
        (None, None) => (DEFAULT_STOREFRONT.to_owned(), StorefrontSource::Default),
    }
}

fn not_found_message(id: u64, storefront: &str, source: StorefrontSource) -> String {
    let hint = match source {
        StorefrontSource::Link => {
            "it may have been removed, or the link's storefront may not carry it"
        }
        StorefrontSource::Input => {
            "catalog availability differs by storefront; try the storefront from the original link"
        }
        StorefrontSource::Default => {
            "catalog availability differs by storefront; pass the storefront from the original link (for example \"jp\")"
        }
    };
    format!("No Apple Music catalog item with id {id} in storefront {storefront:?}: {hint}")
}

fn provider_detail(error: &HttpError) -> String {
    match error {
        HttpError::Status { status, .. } => format!("HTTP {status}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(input: &str) -> CatalogRef {
        match parse_reference(input) {
            Ok(Parsed::Catalog(c)) => c,
            other => panic!("expected catalog ref for {input}: {other:?}"),
        }
    }

    fn unsupported_kind(input: &str) -> String {
        match parse_reference(input) {
            Ok(Parsed::Unsupported { kind, .. }) => kind,
            other => panic!("expected unsupported for {input}: {other:?}"),
        }
    }

    #[test]
    fn bare_ids_have_no_kind_or_storefront() {
        let c = catalog(" 1440818652 ");
        assert_eq!((c.id, c.kind, c.storefront), (1440818652, None, None));
        assert!(matches!(
            parse_reference("0"),
            Err(ReferenceError::NotApple)
        ));
        assert!(matches!(
            parse_reference("1234567890123456"),
            Err(ReferenceError::NotApple)
        ));
        assert!(matches!(parse_reference("  "), Err(ReferenceError::Empty)));
    }

    #[test]
    fn album_links_with_i_select_the_song_and_keep_the_album() {
        let c = catalog("https://music.apple.com/us/album/tennis-court/1440818584?i=1440818652");
        assert_eq!(c.id, 1440818652);
        assert_eq!(c.kind, Some(MusicType::Song));
        assert_eq!(c.album_id, Some(1440818584));
        assert_eq!(c.storefront.as_deref(), Some("us"));
        assert!(matches!(
            parse_reference("https://music.apple.com/us/album/x/1440818584?i=abc"),
            Err(ReferenceError::Malformed(_))
        ));
    }

    #[test]
    fn recognizes_each_supported_path_shape() {
        let album = catalog("https://music.apple.com/GB/album/pure-heroine/1440818584?l=en-GB");
        assert_eq!(album.kind, Some(MusicType::Album));
        assert_eq!(album.storefront.as_deref(), Some("gb"));
        assert_eq!(
            catalog("music.apple.com/jp/song/x/1440818652").kind,
            Some(MusicType::Song)
        );
        assert_eq!(
            catalog("https://music.apple.com/us/artist/lorde/602767352").id,
            602767352
        );
        let video = catalog("https://music.apple.com/us/music-video/royals-edit/1445856407");
        assert_eq!(video.kind, Some(MusicType::MusicVideo));
        let bare = catalog("https://music.apple.com/album/1440818584");
        assert_eq!((bare.storefront, bare.kind), (None, Some(MusicType::Album)));
        let legacy = catalog("https://itunes.apple.com/us/album/pure-heroine/id1440818584");
        assert_eq!(legacy.id, 1440818584);
        assert_eq!(
            catalog("https://geo.music.apple.com/us/album/_/1440818584?app=music").id,
            1440818584
        );
        assert_eq!(
            catalog("https://classical.music.apple.com/us/album/1440818584").id,
            1440818584
        );
        assert_eq!(
            catalog("https://geo.itunes.apple.com/us/album/id1440818584").id,
            1440818584
        );
        // A `://` inside a scheme-less link's query is not its scheme.
        let embedded = catalog("music.apple.com/us/album/x/1440818584?u=https://y.example");
        assert_eq!(embedded.id, 1440818584);
    }

    #[test]
    fn unsupported_links_are_named_without_fetching() {
        assert_eq!(
            unsupported_kind(
                "https://music.apple.com/us/playlist/todays-hits/pl.f4d106fed2bd41149aaacabb233eb5eb"
            ),
            "playlist"
        );
        assert_eq!(
            unsupported_kind("https://music.apple.com/library/playlist/p.abc"),
            "library"
        );
        assert_eq!(
            unsupported_kind("https://music.apple.com/us/station/x/ra.123"),
            "station"
        );
        assert_eq!(unsupported_kind("https://apple.co/3abcDEF"), "short_link");
    }

    #[test]
    fn rejects_non_apple_hosts_and_tricks() {
        for input in [
            "https://music.apple.com.evil.example/us/album/x/1",
            "https://evil.example/music.apple.com/us/album/x/1",
            "https://user@music.apple.com/us/album/x/1",
            "https://music.apple.com:8443/us/album/x/1",
            "ftp://music.apple.com/us/album/x/1",
            "javascript:alert(1)",
            "http://169.254.169.254/latest/meta-data",
            "spotify:track:abc",
        ] {
            assert!(
                matches!(parse_reference(input), Err(ReferenceError::NotApple)),
                "{input}"
            );
        }
        assert!(matches!(
            parse_reference("https://music.apple.com/us/album/no-id"),
            Err(ReferenceError::Malformed(_))
        ));
        assert!(matches!(
            parse_reference("https://music.apple.com/"),
            Err(ReferenceError::Malformed(_))
        ));
    }

    #[test]
    fn explicit_types_pick_the_album_or_conflict() {
        let song = catalog("https://music.apple.com/us/album/x/1440818584?i=1440818652");
        assert_eq!(
            target(&song, Some(MusicType::Album)),
            Ok((1440818584, Some(MusicType::Album)))
        );
        assert_eq!(target(&song, None), Ok((1440818652, Some(MusicType::Song))));
        let artist = catalog("https://music.apple.com/us/artist/x/602767352");
        assert!(matches!(
            target(&artist, Some(MusicType::Song)),
            Err(ReferenceError::TypeConflict { .. })
        ));
        let bare = catalog("136975");
        assert_eq!(
            target(&bare, Some(MusicType::Album)),
            Ok((136975, Some(MusicType::Album)))
        );
    }

    #[test]
    fn storefronts_are_two_letters() {
        assert_eq!(normalize_storefront(" JP "), Ok("jp".to_owned()));
        assert!(normalize_storefront("usa").is_err());
        assert!(normalize_storefront("u1").is_err());
    }

    #[test]
    fn links_lose_tracking_parameters_and_foreign_hosts() {
        assert_eq!(
            clean_link(
                Some("https://music.apple.com/us/album/tennis-court/1440818584?i=1440818652&uo=4"),
                false
            )
            .as_deref(),
            Some("https://music.apple.com/us/album/tennis-court/1440818584?i=1440818652")
        );
        assert_eq!(
            clean_link(
                Some("https://music.apple.com/us/album/tennis-court/1440818584?i=1440818652&uo=4"),
                true
            )
            .as_deref(),
            Some("https://music.apple.com/us/album/tennis-court/1440818584")
        );
        assert_eq!(clean_link(Some("https://evil.example/x"), false), None);
        assert_eq!(artwork_link(Some("https://evil.example/a.jpg")), None);
    }

    #[test]
    fn classifies_non_music_results_by_their_own_label() {
        let raw = |wrapper: &str, kind: Option<&str>| RawResult {
            wrapper_type: Some(wrapper.to_owned()),
            kind: kind.map(str::to_owned),
            track_id: Some(1),
            ..RawResult::default()
        };
        assert_eq!(
            classify(&raw("track", Some("podcast"))),
            Err("podcast".to_owned())
        );
        assert_eq!(
            classify(&raw("track", Some(&"x".repeat(500)))).map_err(|l| l.len()),
            Err(MAX_LABEL_CHARS)
        );
        assert_eq!(
            classify(&raw("software", Some("software"))),
            Err("software".to_owned())
        );
        assert_eq!(
            classify(&raw("track", Some("music-video"))),
            Ok(MusicType::MusicVideo)
        );
        assert_eq!(
            release_day(Some("2013-06-07T07:00:00Z")).as_deref(),
            Some("2013-06-07")
        );
        assert_eq!(release_day(Some("June 2013")), None);
    }
}
