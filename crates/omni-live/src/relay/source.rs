//! Kick playback over its web API: the channel's live playback URL, its top
//! variant, the variant's media playlist and segments.
//!
//! `kick.com/api/v2` is the endpoint Kick's own web player uses; the public
//! v1 API has no playback URL. The master URL carries a signed token that
//! expires ten minutes after it is issued; the variant playlist outlives it,
//! and sequence numbers stay the same across re-resolves of one broadcast.

use std::time::Duration;

use bytes::Bytes;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method};
use serde::Deserialize;
use url::Url;

use super::playlist::{MediaPlaylist, Variant, parse_media, top_variant};

const API_BASE: &str = "https://kick.com";
const API_MAX_BYTES: usize = 512 * 1024;
const PLAYLIST_MAX_BYTES: usize = 1024 * 1024;
/// A 1080p60 Kick segment is about 2 MB.
const SEGMENT_MAX_BYTES: usize = 32 * 1024 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(10);
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(8);
const SEGMENT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum RelaySourceError {
    #[error("{operation}: {source}")]
    Http {
        operation: &'static str,
        source: HttpError,
    },
    #[error("{operation} returned HTTP {status}")]
    Status {
        operation: &'static str,
        status: u16,
    },
    #[error("{0}")]
    Invalid(&'static str),
}

impl RelaySourceError {
    /// The signed URL expired or the playlist moved: resolve again.
    pub fn needs_resolve(&self) -> bool {
        matches!(self, Self::Status { status, .. } if matches!(status, 401 | 403 | 404 | 410))
    }
}

/// A live channel's resolved top variant.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPlayback {
    /// Kick's livestream id: a new id is a new broadcast.
    pub livestream_id: u64,
    pub variant: Variant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    Live(ResolvedPlayback),
    Offline,
}

#[derive(Deserialize)]
struct LivestreamResponse {
    data: Option<Livestream>,
}

#[derive(Deserialize)]
struct Livestream {
    id: u64,
    playback_url: Option<String>,
}

#[derive(Clone)]
pub struct KickPlaybackSource {
    http: PublicHttpClient,
    api_base: String,
}

impl KickPlaybackSource {
    pub fn new(http: PublicHttpClient) -> Self {
        Self::with_api_base(http, API_BASE)
    }

    /// Points the livestream lookup elsewhere (tests).
    pub fn with_api_base(http: PublicHttpClient, base: &str) -> Self {
        Self {
            http,
            api_base: base.trim_end_matches('/').to_owned(),
        }
    }

    async fn get(
        &self,
        operation: &'static str,
        url: Url,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<Bytes, RelaySourceError> {
        let response = self
            .http
            .request(Method::GET, url)
            .timeout(timeout)
            .send_bounded(max_bytes)
            .await
            .map_err(|source| RelaySourceError::Http { operation, source })?;
        let status = response.status.as_u16();
        if !(200..300).contains(&status) {
            return Err(RelaySourceError::Status { operation, status });
        }
        Ok(response.body)
    }

    /// The channel's current broadcast and its top variant.
    pub async fn resolve(&self, slug: &str) -> Result<Resolution, RelaySourceError> {
        let mut url = Url::parse(&self.api_base)
            .map_err(|_| RelaySourceError::Invalid("Kick API base is not a URL"))?;
        url.path_segments_mut()
            .map_err(|()| RelaySourceError::Invalid("Kick API base cannot hold a path"))?
            .extend(["api", "v2", "channels", slug, "livestream"]);
        let body = match self
            .get("Kick livestream lookup", url, API_TIMEOUT, API_MAX_BYTES)
            .await
        {
            Err(RelaySourceError::Status { status: 404, .. }) => return Ok(Resolution::Offline),
            other => other?,
        };
        let decoded: LivestreamResponse = serde_json::from_slice(&body).map_err(|_| {
            RelaySourceError::Invalid("Kick livestream lookup returned unexpected JSON")
        })?;
        let Some(live) = decoded.data else {
            return Ok(Resolution::Offline);
        };
        let Some(playback) = live.playback_url.filter(|u| !u.is_empty()) else {
            return Ok(Resolution::Offline);
        };
        let master_url = Url::parse(&playback)
            .map_err(|_| RelaySourceError::Invalid("Kick playback URL is not a URL"))?;
        let master = self
            .get(
                "Kick master playlist",
                master_url.clone(),
                PLAYLIST_TIMEOUT,
                PLAYLIST_MAX_BYTES,
            )
            .await?;
        let variant = top_variant(&String::from_utf8_lossy(&master), &master_url).ok_or(
            RelaySourceError::Invalid("Kick master playlist lists no variant"),
        )?;
        Ok(Resolution::Live(ResolvedPlayback {
            livestream_id: live.id,
            variant,
        }))
    }

    pub async fn media_playlist(&self, url: &Url) -> Result<MediaPlaylist, RelaySourceError> {
        let body = self
            .get(
                "Kick media playlist",
                url.clone(),
                PLAYLIST_TIMEOUT,
                PLAYLIST_MAX_BYTES,
            )
            .await?;
        parse_media(&String::from_utf8_lossy(&body), url)
            .ok_or(RelaySourceError::Invalid("Kick media playlist is not HLS"))
    }

    pub async fn segment(&self, url: &Url) -> Result<Bytes, RelaySourceError> {
        self.get(
            "Kick segment",
            url.clone(),
            SEGMENT_TIMEOUT,
            SEGMENT_MAX_BYTES,
        )
        .await
    }
}
