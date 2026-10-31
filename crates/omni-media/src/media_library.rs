//! Plex-backed view of the local media library.
//!
//! An unavailable Plex instance is deliberately different from an empty
//! library: callers must not make recommendation decisions from missing state.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::error::IntegrationError;
use crate::plex::PlexClient;
use crate::types::{FetchResult, InProgressItem, MediaItem, WatchedItem};

/// History, in-progress and library views; every failure is `Unavailable`.
pub trait MediaLibrary: Send + Sync {
    fn watch_history(&self) -> BoxFuture<'_, FetchResult<Vec<WatchedItem>>>;
    fn in_progress(&self) -> BoxFuture<'_, FetchResult<Vec<InProgressItem>>>;
    fn library_index(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>>;
}

fn wrap<T>(result: Result<T, IntegrationError>) -> FetchResult<T> {
    match result {
        Ok(value) => FetchResult::Ok(value),
        Err(error) => FetchResult::unavailable(error.to_string()),
    }
}

/// The production [`MediaLibrary`]; without `PLEX_URL`/`PLEX_TOKEN` every
/// view is unavailable with the configuration reason.
#[derive(Clone)]
pub struct PlexLibrary {
    client: Result<PlexClient, String>,
}

impl PlexLibrary {
    pub fn new(client: PlexClient) -> Self {
        Self { client: Ok(client) }
    }

    /// The Plex client could not be created (missing configuration).
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self {
            client: Err(reason.into()),
        }
    }

    /// The client, or why there is none.
    pub fn client(&self) -> Result<PlexClient, String> {
        self.client.clone()
    }

    /// `createPlexClient(url, token, accountId)` over `get`.
    pub fn from_config(
        url: Option<&str>,
        token: Option<&str>,
        account_id: Option<u64>,
        get: impl FnOnce(&str, String) -> Arc<dyn crate::plex::PlexGet>,
    ) -> Self {
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            return Self::unconfigured("PLEX_URL is not configured");
        };
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Self::unconfigured("PLEX_TOKEN is not configured");
        };
        Self::new(PlexClient::new(get(url, token.to_owned()), account_id))
    }
}

impl MediaLibrary for PlexLibrary {
    fn watch_history(&self) -> BoxFuture<'_, FetchResult<Vec<WatchedItem>>> {
        Box::pin(async move {
            match &self.client {
                Ok(client) => wrap(client.fetch_watch_history().await),
                Err(reason) => FetchResult::unavailable(reason.clone()),
            }
        })
    }

    fn in_progress(&self) -> BoxFuture<'_, FetchResult<Vec<InProgressItem>>> {
        Box::pin(async move {
            match &self.client {
                Ok(client) => wrap(client.fetch_in_progress().await),
                Err(reason) => FetchResult::unavailable(reason.clone()),
            }
        })
    }

    fn library_index(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>> {
        Box::pin(async move {
            match &self.client {
                Ok(client) => wrap(client.fetch_library_index().await),
                Err(reason) => FetchResult::unavailable(reason.clone()),
            }
        })
    }
}
