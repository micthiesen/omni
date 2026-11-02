//! External lookup seams used by discovery and resolution: web search (Tavily),
//! the show directory (iTunes + RSS), and their HTTP implementations.

use futures::future::BoxFuture;
use jiff::tz::TimeZone;
use omni_ai::tools::{SearchOptions, WebSearch, WebSearchResults};
use omni_http::public::PublicHttpClient;

use crate::itunes::{self, ItunesShow};
use crate::rss::{self, FeedEpisode};

/// Web search; errors are the failure message.
pub trait WebSearcher: Send + Sync {
    fn search(&self, options: SearchOptions) -> BoxFuture<'_, Result<WebSearchResults, String>>;
}

impl WebSearcher for WebSearch {
    fn search(&self, options: SearchOptions) -> BoxFuture<'_, Result<WebSearchResults, String>> {
        Box::pin(async move {
            WebSearch::search(self, options)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Show identity (iTunes) and verified episodes (the show's RSS feed).
pub trait ShowDirectory: Send + Sync {
    fn search_itunes<'a>(&'a self, term: &'a str)
    -> BoxFuture<'a, Result<Vec<ItunesShow>, String>>;
    fn fetch_feed<'a>(
        &'a self,
        feed_url: &'a str,
        max_episodes: usize,
    ) -> BoxFuture<'a, Result<Vec<FeedEpisode>, String>>;
}

/// The production directory over the SSRF-guarded public client.
pub struct HttpDirectory {
    pub http: PublicHttpClient,
    pub tz: TimeZone,
}

impl ShowDirectory for HttpDirectory {
    fn search_itunes<'a>(
        &'a self,
        term: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ItunesShow>, String>> {
        Box::pin(async move {
            itunes::search_itunes_podcasts(
                &self.http,
                term,
                itunes::DEFAULT_LIMIT,
                itunes::ITUNES_RESPONSE_MAX_BYTES,
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn fetch_feed<'a>(
        &'a self,
        feed_url: &'a str,
        max_episodes: usize,
    ) -> BoxFuture<'a, Result<Vec<FeedEpisode>, String>> {
        Box::pin(async move {
            rss::fetch_feed_episodes(&self.http, feed_url, max_episodes, &self.tz)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// A directory that reads nothing (tests and offline audits).
pub struct OfflineDirectory;

impl ShowDirectory for OfflineDirectory {
    fn search_itunes<'a>(
        &'a self,
        _term: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ItunesShow>, String>> {
        Box::pin(async { Err("show directory is offline".to_owned()) })
    }

    fn fetch_feed<'a>(
        &'a self,
        _feed_url: &'a str,
        _max_episodes: usize,
    ) -> BoxFuture<'a, Result<Vec<FeedEpisode>, String>> {
        Box::pin(async { Err("show directory is offline".to_owned()) })
    }
}
