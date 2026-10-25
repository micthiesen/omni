//! Shared fakes for the media tests: every I/O seam is in-process, so no
//! test reaches Plex, TMDB, Radarr, Sonarr, Tavily or Pushover.
#![allow(dead_code, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_http::SideEffectMode;
use omni_media::arr::inspect::ArrInspect;
use omni_media::arr::movies::RadarrMovies;
use omni_media::arr::queue::ArrQueue;
use omni_media::arr::seasons::SonarrSeasons;
use omni_media::arr::{ArrConfig, ArrHttp};
use omni_media::error::IntegrationError;
use omni_media::media_library::MediaLibrary;
use omni_media::selection::{Research, ResearchHit};
use omni_media::services::{MediaServices, Notifier, RecommendationPush};
use omni_media::tmdb::types::{TmdbTitle, TmdbTitleDetails};
use omni_media::tmdb::{Catalog, CatalogResult, DiscoverOptions, FindSource, GenreMap};
use omni_media::types::{
    FetchResult, InProgressItem, MediaItem, MediaType, WatchedItem, WatchlistAddOutcome,
};
use omni_media::watchlist::{Watchlist, WatchlistAddRequest};
use omni_testkit::TestApp;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone)]
pub struct FakeLibrary {
    pub history: Arc<Mutex<FetchResult<Vec<WatchedItem>>>>,
    pub in_progress: Arc<Mutex<FetchResult<Vec<InProgressItem>>>>,
    pub library: Arc<Mutex<FetchResult<Vec<MediaItem>>>>,
    pub calls: Arc<Mutex<u32>>,
}

impl Default for FakeLibrary {
    fn default() -> Self {
        Self {
            history: Arc::new(Mutex::new(FetchResult::Ok(Vec::new()))),
            in_progress: Arc::new(Mutex::new(FetchResult::Ok(Vec::new()))),
            library: Arc::new(Mutex::new(FetchResult::Ok(Vec::new()))),
            calls: Arc::new(Mutex::new(0)),
        }
    }
}

impl MediaLibrary for FakeLibrary {
    fn watch_history(&self) -> BoxFuture<'_, FetchResult<Vec<WatchedItem>>> {
        *lock(&self.calls) += 1;
        let value = lock(&self.history).clone();
        Box::pin(async move { value })
    }
    fn in_progress(&self) -> BoxFuture<'_, FetchResult<Vec<InProgressItem>>> {
        let value = lock(&self.in_progress).clone();
        Box::pin(async move { value })
    }
    fn library_index(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>> {
        let value = lock(&self.library).clone();
        Box::pin(async move { value })
    }
}

#[derive(Clone)]
pub struct FakeWatchlist {
    pub items: Arc<Mutex<FetchResult<Vec<MediaItem>>>>,
    pub add_results: Arc<Mutex<VecDeque<WatchlistAddOutcome>>>,
    pub adds: Arc<Mutex<Vec<WatchlistAddRequest>>>,
}

impl Default for FakeWatchlist {
    fn default() -> Self {
        Self {
            items: Arc::new(Mutex::new(FetchResult::Ok(Vec::new()))),
            add_results: Arc::new(Mutex::new(VecDeque::new())),
            adds: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeWatchlist {
    pub fn adds(&self) -> Vec<WatchlistAddRequest> {
        lock(&self.adds).clone()
    }
    pub fn always(&self, outcome: WatchlistAddOutcome) {
        let mut queue = lock(&self.add_results);
        queue.clear();
        for _ in 0..32 {
            queue.push_back(outcome.clone());
        }
    }
}

impl Watchlist for FakeWatchlist {
    fn fetch(&self) -> BoxFuture<'_, FetchResult<Vec<MediaItem>>> {
        let value = lock(&self.items).clone();
        Box::pin(async move { value })
    }
    fn add<'a>(&'a self, request: &'a WatchlistAddRequest) -> BoxFuture<'a, WatchlistAddOutcome> {
        lock(&self.adds).push(request.clone());
        let outcome = lock(&self.add_results)
            .pop_front()
            .unwrap_or(WatchlistAddOutcome {
                result: omni_media::types::AddToWatchlistResult::Error,
                title_slug: None,
            });
        Box::pin(async move { outcome })
    }
}

type GenreIds = HashMap<(MediaType, i64), Vec<i64>>;

type DetailsFn =
    dyn Fn(MediaType, i64) -> BoxFuture<'static, CatalogResult<TmdbTitleDetails>> + Send + Sync;

/// A scripted TMDB catalog; unscripted calls return empty results.
#[derive(Clone, Default)]
pub struct FakeCatalog {
    pub search: Arc<Mutex<Vec<TmdbTitle>>>,
    pub find: Arc<Mutex<HashMap<String, CatalogResult<Vec<TmdbTitle>>>>>,
    pub genres: Arc<Mutex<GenreIds>>,
    pub details: Arc<Mutex<Option<Arc<DetailsFn>>>>,
    pub genre_names: Arc<Mutex<HashMap<i64, String>>>,
    pub trending: Arc<Mutex<Vec<TmdbTitle>>>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FakeCatalog {
    pub fn calls(&self) -> Vec<String> {
        lock(&self.calls).clone()
    }
    pub fn set_details(&self, f: Arc<DetailsFn>) {
        *lock(&self.details) = Some(f);
    }
    fn record(&self, call: String) {
        lock(&self.calls).push(call);
    }
}

impl Catalog for FakeCatalog {
    fn search_titles<'a>(
        &'a self,
        query: &'a str,
        media_type: MediaType,
        year: Option<i64>,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        self.record(format!("search:{query}:{}:{year:?}", media_type.as_str()));
        let value = lock(&self.search).clone();
        Box::pin(async move { Ok(value) })
    }
    fn find_by_external_id<'a>(
        &'a self,
        external_id: &'a str,
        source: FindSource,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        self.record(format!("find:{external_id}:{}", source.as_str()));
        let value = lock(&self.find)
            .get(external_id)
            .cloned()
            .unwrap_or(Ok(Vec::new()));
        Box::pin(async move { value })
    }
    fn recommendations_for(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>> {
        self.record(format!("recommendations:{}:{tmdb_id}", media_type.as_str()));
        Box::pin(async { Ok(Vec::new()) })
    }
    fn discover<'a>(
        &'a self,
        media_type: MediaType,
        _options: &'a DiscoverOptions,
    ) -> BoxFuture<'a, CatalogResult<Vec<TmdbTitle>>> {
        self.record(format!("discover:{}", media_type.as_str()));
        Box::pin(async { Ok(Vec::new()) })
    }
    fn trending(&self) -> BoxFuture<'_, CatalogResult<Vec<TmdbTitle>>> {
        self.record("trending".to_owned());
        let value = lock(&self.trending).clone();
        Box::pin(async move { Ok(value) })
    }
    fn title_genre_ids(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<Vec<i64>>> {
        self.record(format!("genres:{}:{tmdb_id}", media_type.as_str()));
        let value = lock(&self.genres)
            .get(&(media_type, tmdb_id))
            .cloned()
            .unwrap_or_default();
        Box::pin(async move { Ok(value) })
    }
    fn title_details(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> BoxFuture<'_, CatalogResult<TmdbTitleDetails>> {
        self.record(format!("details:{}:{tmdb_id}", media_type.as_str()));
        match lock(&self.details).clone() {
            Some(f) => f(media_type, tmdb_id),
            None => Box::pin(async { Ok(TmdbTitleDetails::default()) }),
        }
    }
    fn genre_map(&self, _media_type: MediaType) -> BoxFuture<'_, CatalogResult<GenreMap>> {
        let map = Arc::new(lock(&self.genre_names).clone());
        Box::pin(async move { Ok(map) })
    }
}

/// Integration error shorthand.
pub fn failure(operation: &str, cause: &str) -> IntegrationError {
    IntegrationError::new(operation, cause)
}

#[derive(Clone, Default)]
pub struct FakeResearch {
    pub queries: Arc<Mutex<Vec<String>>>,
}

impl Research for FakeResearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        _max_results: u32,
        _max_content_chars: usize,
    ) -> BoxFuture<'a, Result<Vec<ResearchHit>, String>> {
        lock(&self.queries).push(query.to_owned());
        Box::pin(async { Ok(Vec::new()) })
    }
}

type NotifyHook = dyn Fn(RecommendationPush) -> BoxFuture<'static, ()> + Send + Sync;

#[derive(Clone, Default)]
pub struct RecordingNotifier {
    pub pushes: Arc<Mutex<Vec<RecommendationPush>>>,
    pub fail: Arc<Mutex<bool>>,
    pub hook: Arc<Mutex<Option<Arc<NotifyHook>>>>,
}

impl RecordingNotifier {
    pub fn pushes(&self) -> Vec<RecommendationPush> {
        lock(&self.pushes).clone()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, push: RecommendationPush) -> BoxFuture<'_, Result<(), String>> {
        let hook = lock(&self.hook).clone();
        lock(&self.pushes).push(push.clone());
        let fail = *lock(&self.fail);
        Box::pin(async move {
            if let Some(hook) = hook {
                hook(push).await;
            }
            if fail {
                Err("pushover unavailable".to_owned())
            } else {
                Ok(())
            }
        })
    }
}

/// Fakes plus the services built over a [`TestApp`].
pub struct Harness {
    pub app: TestApp,
    pub library: FakeLibrary,
    pub watchlist: FakeWatchlist,
    pub catalog: FakeCatalog,
    pub research: FakeResearch,
    pub notifier: RecordingNotifier,
    pub services: MediaServices,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_app(TestApp::new().await)
    }

    pub fn with_app(app: TestApp) -> Self {
        let library = FakeLibrary::default();
        let watchlist = FakeWatchlist::default();
        let catalog = FakeCatalog::default();
        let research = FakeResearch::default();
        let notifier = RecordingNotifier::default();
        let services = MediaServices {
            config: app.ctx.config.clone(),
            store: app.ctx.store.clone(),
            clock: app.ctx.clock.clone(),
            ai: app.ctx.ai.clone(),
            library: Arc::new(library.clone()),
            watchlist: Arc::new(watchlist.clone()),
            seasons: sonarr_seasons(&app, ArrConfig::default(), SideEffectMode::Live),
            movies: radarr_movies(&app, ArrConfig::default(), SideEffectMode::Live),
            arr: arr_inspect(ArrConfig::default(), ArrConfig::default()),
            queue: arr_queue(&app, ArrConfig::default(), ArrConfig::default()),
            nzbget: omni_media::nzbget::NzbGet::new(omni_testkit::no_network(), None),
            catalog: Arc::new(catalog.clone()),
            research: Arc::new(research.clone()),
            notifier: Arc::new(notifier.clone()),
        };
        Self {
            app,
            library,
            watchlist,
            catalog,
            research,
            notifier,
            services,
        }
    }
}

/// Season management over `config` (an unconfigured Sonarr by default).
pub fn sonarr_seasons(app: &TestApp, config: ArrConfig, mode: SideEffectMode) -> SonarrSeasons {
    SonarrSeasons::new(
        ArrHttp::new(omni_testkit::no_network(), mode),
        config,
        app.ctx.store.clone(),
        app.ctx.tracker.clone(),
    )
}

/// Movie management over `config` (an unconfigured Radarr by default).
pub fn radarr_movies(app: &TestApp, config: ArrConfig, mode: SideEffectMode) -> RadarrMovies {
    RadarrMovies::new(
        ArrHttp::new(omni_testkit::no_network(), mode),
        config,
        app.ctx.store.clone(),
        app.ctx.tracker.clone(),
    )
}

/// The shared Radarr/Sonarr reads over `radarr` and `sonarr`.
pub fn arr_inspect(radarr: ArrConfig, sonarr: ArrConfig) -> ArrInspect {
    ArrInspect::new(
        ArrHttp::new(omni_testkit::no_network(), SideEffectMode::Live),
        radarr,
        sonarr,
    )
}

/// Queue removals over `radarr` and `sonarr`.
pub fn arr_queue(app: &TestApp, radarr: ArrConfig, sonarr: ArrConfig) -> ArrQueue {
    ArrQueue::new(
        arr_inspect(radarr, sonarr),
        app.ctx.store.clone(),
        app.ctx.tracker.clone(),
    )
}

pub fn media(guid: &str, title: &str, media_type: MediaType, tmdb: Option<i64>) -> MediaItem {
    MediaItem {
        guid: guid.to_owned(),
        title: title.to_owned(),
        year: None,
        media_type,
        external_ids: Some(omni_media::types::ExternalIds {
            tmdb,
            ..Default::default()
        }),
        title_slug: None,
    }
}

pub fn tmdb_title(tmdb_id: i64, media_type: MediaType) -> TmdbTitle {
    TmdbTitle {
        tmdb_id,
        media_type,
        title: format!("Title {tmdb_id}"),
        overview: String::new(),
        vote_average: 7.0,
        vote_count: 500.0,
        popularity: 5.0,
        original_language: Some("en".to_owned()),
        ..TmdbTitle::default()
    }
}

/// A one-connection raw HTTP server for body-streaming edge cases: it reads
/// the request head, writes `head` and then each chunk of `body_chunks`, and
/// reports (through the returned receiver) once the client closed the socket.
pub async fn raw_http_server(
    head: String,
    body_chunks: Vec<Vec<u8>>,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        if socket.write_all(head.as_bytes()).await.is_err() {
            let _ = closed_tx.send(());
            return;
        }
        for chunk in body_chunks {
            if socket.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = socket.flush().await;
        let _ = sent_tx.send(());
        // Hold the connection open until the client goes away.
        loop {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = closed_tx.send(());
    });
    (format!("http://{addr}"), sent_rx, closed_rx)
}

/// A chunked-encoding frame for `data`.
pub fn chunk(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// The text of a fake model request's prompt.
pub fn prompt_text(request: &omni_ai::GenerateRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|part| match part {
            omni_ai::ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
