//! Kick live status over the public API with client-credentials OAuth.

use std::sync::Arc;

use omni_core::clock::SharedClock;
use omni_http::public::PublicHttpClient;
use omni_http::{HttpError, Method};
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

use super::{PLATFORM_TIMEOUT, request_failure};
use crate::platform::{FetchedLive, FetchedStatus, js_count};

const TOKEN_URL: &str = "https://id.kick.com/oauth/token";
const CHANNELS_URL: &str = "https://api.kick.com/public/v1/channels";
const USERS_URL: &str = "https://api.kick.com/public/v1/users";
const TOKEN_REFRESH_LEEWAY_MS: i64 = 60_000;
/// Token response cap.
pub const KICK_TOKEN_MAX_BYTES: usize = 128 * 1024;
/// Channels response cap.
pub const KICK_CHANNELS_MAX_BYTES: usize = 2 * 1024 * 1024;

/// `KICK_CLIENT_ID` / `KICK_CLIENT_SECRET`.
#[derive(Clone)]
pub struct KickCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for KickCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KickCredentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: f64,
}

#[derive(Clone, Debug)]
struct CachedToken {
    access_token: String,
    expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct KickChannelsResponse {
    pub data: Vec<KickChannel>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KickChannel {
    pub slug: String,
    #[serde(default)]
    pub stream_title: String,
    #[serde(default)]
    pub category: Option<KickCategory>,
    #[serde(default)]
    pub stream: Option<KickStream>,
}

#[derive(Debug, Deserialize)]
pub struct KickCategory {
    #[serde(default)]
    pub id: Option<f64>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KickStream {
    pub is_live: bool,
    #[serde(default)]
    pub viewer_count: Option<f64>,
    #[serde(default)]
    pub start_time: Option<String>,
}

/// The channel fields the avatar lookup needs.
#[derive(Debug, Deserialize)]
struct KickChannelOwners {
    data: Vec<KickChannelOwner>,
}

#[derive(Debug, Deserialize)]
struct KickChannelOwner {
    #[serde(default)]
    broadcaster_user_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct KickUsers {
    data: Vec<KickUser>,
}

#[derive(Debug, Deserialize)]
struct KickUser {
    #[serde(default)]
    profile_picture: Option<String>,
}

/// Pure: the first channel's status.
pub fn extract_kick_status(response: &KickChannelsResponse) -> FetchedStatus {
    let Some(channel) = response.data.first() else {
        return FetchedStatus::Offline;
    };
    let Some(stream) = channel.stream.as_ref().filter(|s| s.is_live) else {
        return FetchedStatus::Offline;
    };
    let title = if channel.stream_title.is_empty() {
        channel.slug.clone()
    } else {
        channel.stream_title.clone()
    };
    FetchedStatus::Live(FetchedLive {
        title,
        viewer_count: stream.viewer_count.and_then(js_count),
        category: channel.category.as_ref().and_then(|c| c.name.clone()),
        started_at: None,
    })
}

struct RawResponse {
    status: u16,
    body: String,
}

fn snippet(body: &str) -> String {
    omni_core::js::utf16_slice(body, 0, 200).into_owned()
}

/// Kick client with a serialized token cache: a burst of callers that see an
/// expired token performs one exchange.
#[derive(Clone)]
pub struct KickClient {
    http: PublicHttpClient,
    credentials: Option<KickCredentials>,
    clock: Option<SharedClock>,
    token: Arc<Mutex<Option<CachedToken>>>,
    token_max_bytes: usize,
    channels_max_bytes: usize,
}

impl KickClient {
    pub fn new(http: PublicHttpClient, credentials: Option<KickCredentials>) -> Self {
        Self {
            http,
            credentials,
            clock: None,
            token: Arc::default(),
            token_max_bytes: KICK_TOKEN_MAX_BYTES,
            channels_max_bytes: KICK_CHANNELS_MAX_BYTES,
        }
    }

    /// Token expiry uses this clock (default: system time).
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Response caps (tests).
    pub fn with_limits(mut self, token_max_bytes: usize, channels_max_bytes: usize) -> Self {
        self.token_max_bytes = token_max_bytes;
        self.channels_max_bytes = channels_max_bytes;
        self
    }

    fn now_ms(&self) -> i64 {
        match &self.clock {
            Some(clock) => clock.now_ms(),
            None => jiff::Timestamp::now().as_millisecond(),
        }
    }

    pub async fn fetch_live_status(&self, username: &str) -> FetchedStatus {
        match self.try_fetch(username).await {
            Ok(status) => status,
            Err(error) => FetchedStatus::unknown(error),
        }
    }

    async fn try_fetch(&self, username: &str) -> Result<FetchedStatus, String> {
        let response = self
            .authorized_get("Kick channels request", CHANNELS_URL, ("slug", username))
            .await?;
        if !(200..300).contains(&response.status) {
            return Ok(FetchedStatus::unknown(format!(
                "Kick API returned {}: {}",
                response.status,
                snippet(&response.body)
            )));
        }
        let decoded: KickChannelsResponse = serde_json::from_str(&response.body)
            .map_err(|e| format!("Decode kick channels request: {e}"))?;
        Ok(extract_kick_status(&decoded))
    }

    /// Whether Kick credentials are configured (the API needs them).
    pub fn has_credentials(&self) -> bool {
        self.credentials
            .as_ref()
            .is_some_and(|c| !c.client_id.is_empty() && !c.client_secret.is_empty())
    }

    /// The channel owner's profile picture: the channel's
    /// `broadcaster_user_id`, then `/users?id=`. `None` for an unknown slug or
    /// a user without a picture.
    pub async fn fetch_avatar(&self, username: &str) -> Result<Option<String>, String> {
        let channels = self
            .authorized_get("Kick channels request", CHANNELS_URL, ("slug", username))
            .await?;
        let channels: KickChannelOwners = decode_ok("Kick channels", &channels)?;
        let Some(user_id) = channels.data.first().and_then(|c| c.broadcaster_user_id) else {
            return Ok(None);
        };
        let id = user_id.to_string();
        let users = self
            .authorized_get("Kick users request", USERS_URL, ("id", &id))
            .await?;
        let users: KickUsers = decode_ok("Kick users", &users)?;
        Ok(users
            .data
            .into_iter()
            .next()
            .and_then(|user| user.profile_picture)
            .filter(|url| !url.is_empty()))
    }

    /// GET with the cached app token, refreshed once on a 401.
    async fn authorized_get(
        &self,
        operation: &str,
        url: &str,
        query: (&str, &str),
    ) -> Result<RawResponse, String> {
        let mut token = self.access_token(None).await?;
        let mut response = self.request(operation, url, query, &token).await?;
        if response.status == 401 {
            token = self.access_token(Some(&token)).await?;
            response = self.request(operation, url, query, &token).await?;
        }
        Ok(response)
    }

    async fn access_token(&self, stale: Option<&str>) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        let now = self.now_ms();
        if let Some(token) = cached.as_ref()
            && token.expires_at > now
            && Some(token.access_token.as_str()) != stale
        {
            return Ok(token.access_token.clone());
        }
        let fresh = self.exchange_token().await?;
        let value = fresh.access_token.clone();
        *cached = Some(fresh);
        Ok(value)
    }

    async fn exchange_token(&self) -> Result<CachedToken, String> {
        let Some(credentials) = self
            .credentials
            .as_ref()
            .filter(|c| !c.client_id.is_empty() && !c.client_secret.is_empty())
        else {
            return Err(
                "Kick token configuration: KICK_CLIENT_ID and KICK_CLIENT_SECRET env vars are required for Kick"
                    .to_owned(),
            );
        };
        let operation = "Kick token request";
        let url = Url::parse(TOKEN_URL).map_err(|e| format!("{operation}: {e}"))?;
        let request = self.http.request(Method::POST, url).form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &credentials.client_id),
            ("client_secret", &credentials.client_secret),
        ]);
        let response = send(request, self.token_max_bytes)
            .await
            .map_err(|e| request_failure(operation, &e))?;
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "{operation}: Kick token API returned {}: {}",
                response.status,
                snippet(&response.body)
            ));
        }
        let parsed: TokenResponse = serde_json::from_str(&response.body)
            .map_err(|e| format!("Decode kick token request: {e}"))?;
        #[allow(clippy::cast_possible_truncation)]
        let lifetime_ms = (parsed.expires_in * 1_000.0) as i64;
        omni_core::secrets::register_runtime(&parsed.access_token);
        Ok(CachedToken {
            access_token: parsed.access_token,
            expires_at: self.now_ms() + lifetime_ms - TOKEN_REFRESH_LEEWAY_MS,
        })
    }

    async fn request(
        &self,
        operation: &str,
        url: &str,
        query: (&str, &str),
        bearer: &str,
    ) -> Result<RawResponse, String> {
        let url = Url::parse(url).map_err(|e| format!("{operation}: {e}"))?;
        let request = self
            .http
            .request(Method::GET, url)
            .query(&[query])
            .bearer_auth(bearer);
        send(request, self.channels_max_bytes)
            .await
            .map_err(|e| request_failure(operation, &e))
    }
}

/// A 2xx body decoded into `T`.
fn decode_ok<T: serde::de::DeserializeOwned>(
    operation: &str,
    response: &RawResponse,
) -> Result<T, String> {
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "{operation} returned {}: {}",
            response.status,
            snippet(&response.body)
        ));
    }
    serde_json::from_str(&response.body).map_err(|e| format!("Decode {operation} response: {e}"))
}

async fn send(
    request: omni_http::RequestBuilder,
    max_bytes: usize,
) -> Result<RawResponse, HttpError> {
    let response = request
        .timeout(PLATFORM_TIMEOUT)
        .send_bounded(max_bytes)
        .await?;
    Ok(RawResponse {
        status: response.status.as_u16(),
        body: String::from_utf8_lossy(&response.body).into_owned(),
    })
}
