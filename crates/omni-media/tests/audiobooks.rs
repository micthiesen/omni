//! Audiobooks end to end: ranked MyAnonamouse search, a grab that is added to
//! Deluge once and only into the Plex library's folder, a watcher that
//! scans Plex and notifies once when the book is ready, and cancel limited to
//! unfinished downloads.
#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_http::SideEffectMode;
use omni_media::audiobooks::deluge::{AddError, Added, DelugeError, TorrentClient, TorrentStatus};
use omni_media::audiobooks::{
    Audiobooks, AudiobooksConfig, CancelOutcome, GrabOutcome, GrabRequest, Problem, Recorded,
    SearchResult, Stage,
};
use omni_testkit::{TEST_EPOCH_MS, TestApp};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const QUERY: &str = "Project Hail Mary";
const BOOKS: &str = "/media/storage/deluge/books";
const HOUR_MS: i64 = 60 * 60 * 1000;

/// A minimal single-file torrent named `name`.
fn torrent(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi9e4:name{}:{name}12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaae",
        name.len()
    );
    format!("d8:announce10:http://t/a4:info{info}e").into_bytes()
}

fn hash_of(name: &str) -> String {
    omni_media::audiobooks::torrent::parse(&torrent(name))
        .expect("torrent")
        .info_hash
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddBehavior {
    Add,
    /// The add reaches Deluge but the answer is lost.
    AddButLoseAnswer,
    LoseAnswer,
    /// Deluge already has the torrent from elsewhere.
    AlreadyThere,
}

#[derive(Default)]
struct FakeState {
    label_path: Option<String>,
    torrents: BTreeMap<String, TorrentStatus>,
    adds: Vec<(String, String, String)>,
    removes: Vec<String>,
    behavior: Option<AddBehavior>,
    unreadable: bool,
}

#[derive(Clone, Default)]
struct FakeDeluge(Arc<Mutex<FakeState>>);

impl FakeDeluge {
    fn new() -> Self {
        let fake = Self::default();
        fake.state().label_path = Some(BOOKS.to_owned());
        fake
    }
    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.0.lock().expect("lock")
    }
    fn set(&self, hash: &str, status: TorrentStatus) {
        self.state().torrents.insert(hash.to_owned(), status);
    }
}

impl TorrentClient for FakeDeluge {
    fn is_configured(&self) -> bool {
        true
    }
    fn label_completed_path<'a>(
        &'a self,
        _label: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, DelugeError>> {
        let path = self.state().label_path.clone();
        Box::pin(async move { Ok(path) })
    }
    fn add<'a>(
        &'a self,
        file_name: &'a str,
        _torrent: &'a [u8],
        hash: &'a str,
        label: &'a str,
        completed_path: &'a str,
    ) -> BoxFuture<'a, Result<Added, AddError>> {
        let mut state = self.state();
        state
            .adds
            .push((hash.to_owned(), label.to_owned(), completed_path.to_owned()));
        let behavior = state.behavior.unwrap_or(AddBehavior::Add);
        if behavior != AddBehavior::LoseAnswer {
            state.torrents.insert(
                hash.to_owned(),
                TorrentStatus {
                    name: file_name.trim_end_matches(".torrent").to_owned(),
                    state: "Downloading".to_owned(),
                    save_path: "/media/storage/deluge/incomplete".to_owned(),
                    move_completed_path: completed_path.to_owned(),
                    label: label.to_owned(),
                    ..TorrentStatus::default()
                },
            );
        }
        let result = match behavior {
            AddBehavior::Add => Ok(Added::Added),
            AddBehavior::AlreadyThere => Ok(Added::AlreadyPresent),
            _ => Err(AddError::Unknown(DelugeError::Timeout(45))),
        };
        Box::pin(async move { result })
    }
    fn statuses<'a>(
        &'a self,
        hashes: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, TorrentStatus>, DelugeError>> {
        let state = self.state();
        let result = if state.unreadable {
            Err(DelugeError::Connect("refused".to_owned()))
        } else {
            Ok(state
                .torrents
                .iter()
                .filter(|(h, _)| hashes.contains(h))
                .map(|(h, s)| (h.clone(), s.clone()))
                .collect())
        };
        Box::pin(async move { result })
    }
    fn remove<'a>(&'a self, hash: &'a str) -> BoxFuture<'a, Result<(), DelugeError>> {
        let mut state = self.state();
        state.removes.push(hash.to_owned());
        state.torrents.remove(hash);
        Box::pin(async { Ok(()) })
    }
}

fn mam(title: &str, guid: &str, seeders: i64) -> Value {
    json!({
        "guid": format!("https://www.myanonamouse.net/t/{guid}"), "indexerId": 3,
        "indexer": "MyAnonamouse", "title": title, "size": 900_000_000, "files": 3,
        "seeders": seeders, "leechers": 0, "protocol": "torrent", "indexerFlags": [],
        "downloadUrl": format!("http://127.0.0.1:9696/3/download?apikey=secret&link={guid}"),
        "categories": [{"id": 3030}]
    })
}

async fn prowlarr() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/indexer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "name": "NZBgeek", "enable": true},
            {"id": 3, "name": "MyAnonamouse", "enable": true}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("indexerIds", "3"))
        .and(query_param("categories", "3030"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            mam("Project Hail Mary by Andy Weir [ENG / MP3]", "720148", 1364),
            mam("Project Hail Mary by Andy Weir [DUT / M4B]", "1", 41),
            mam("Project Hail Mary by Andy Weir [ENG / M4B]", "1252065", 95),
            {"title": "broken", "seeders": "many"}
        ])))
        .mount(&server)
        .await;
    for (guid, name) in [("1252065", "Project Hail Mary"), ("720148", "PHM mp3")] {
        Mock::given(method("GET"))
            .and(path("/3/download"))
            .and(query_param("link", guid))
            .and(query_param("apikey", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(torrent(name)))
            .mount(&server)
            .await;
    }
    server
}

/// Plex with the Audiobooks library at `location` holding `albums` and `tracks`.
async fn plex(location: &str, albums: Value, tracks: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/library/sections"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"MediaContainer": {
            "Directory": [
                {"key": "6", "type": "artist", "title": "Music", "Location": [{"path": "/music"}]},
                {"key": "4", "type": "artist", "title": "Audiobooks",
                 "Location": [{"path": "/media/storage/audiobooks"}, {"path": location}]}
            ]
        }})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections/4/all"))
        .and(query_param("type", "9"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"MediaContainer": {"size": 1, "totalSize": 1, "Metadata": albums}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections/4/all"))
        .and(query_param("type", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"MediaContainer": {"size": 1, "totalSize": 1, "Metadata": tracks}}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/library/sections/4/refresh"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

fn books(
    app: &TestApp,
    prowlarr: &MockServer,
    plex: &MockServer,
    deluge: &FakeDeluge,
    mode: SideEffectMode,
) -> Audiobooks {
    let client = omni_media::plex::PlexClient::new(
        Arc::new(omni_media::plex::HttpPlexGet::new(
            omni_testkit::no_network(),
            &plex.uri(),
            "plex-token".to_owned(),
        )),
        None,
    );
    Audiobooks::new(
        omni_testkit::no_network(),
        mode,
        AudiobooksConfig {
            prowlarr_url: Some(prowlarr.uri()),
            prowlarr_api_key: Some("prowlarr-key".to_owned()),
            indexer: "MyAnonamouse".to_owned(),
            language: "ENG".to_owned(),
            library: "Audiobooks".to_owned(),
            label: "books".to_owned(),
            plex_url: Some(plex.uri()),
            plex_token: Some("plex-token".to_owned()),
        },
        Ok(client),
        Arc::new(deluge.clone()),
        app.ctx.store.clone(),
        app.ctx.tracker.clone(),
        app.ctx.pushover.clone(),
    )
}

struct World {
    app: TestApp,
    deluge: FakeDeluge,
    _prowlarr: MockServer,
    plex: MockServer,
    books: Audiobooks,
}

async fn world_with(location: &str, albums: Value, tracks: Value, mode: SideEffectMode) -> World {
    let app = TestApp::new().await;
    let deluge = FakeDeluge::new();
    let prowlarr = prowlarr().await;
    let plex = plex(location, albums, tracks).await;
    let books = books(&app, &prowlarr, &plex, &deluge, mode);
    World {
        app,
        deluge,
        _prowlarr: prowlarr,
        plex,
        books,
    }
}

async fn world() -> World {
    world_with(BOOKS, json!([]), json!([]), SideEffectMode::Live).await
}

async fn best_release(books: &Audiobooks) -> String {
    books.search(QUERY).await.releases[0].release_id.clone()
}

fn grab(release_id: String, force: bool) -> GrabRequest {
    GrabRequest {
        query: QUERY.to_owned(),
        release_id,
        force,
    }
}

fn finished(name: &str) -> TorrentStatus {
    TorrentStatus {
        name: name.to_owned(),
        state: "Seeding".to_owned(),
        progress_percent: 100.0,
        save_path: BOOKS.to_owned(),
        move_completed_path: BOOKS.to_owned(),
        is_finished: true,
        files: vec![format!("{name}/{name}.m4b"), format!("{name}/cover.jpg")],
        ..TorrentStatus::default()
    }
}

#[tokio::test]
async fn search_ranks_english_m4b_first_and_hides_passkeys() {
    let w = world().await;

    let search = w.books.search(QUERY).await;

    assert_eq!(search.result, SearchResult::Listed);
    let titles: Vec<&str> = search.releases.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Project Hail Mary by Andy Weir [ENG / M4B]",
            "Project Hail Mary by Andy Weir [ENG / MP3]",
            "Project Hail Mary by Andy Weir [DUT / M4B]",
        ]
    );
    let best = &search.releases[0];
    assert_eq!(best.rank, 1);
    assert!(best.good_match);
    assert_eq!(best.author.as_deref(), Some("Andy Weir"));
    let wire = serde_json::to_string(&search).expect("json");
    assert!(
        !wire.contains("secret") && !wire.contains("myanonamouse.net/t"),
        "{wire}"
    );
}

#[tokio::test]
async fn a_grab_lands_in_the_library_folder_and_is_never_added_twice() {
    let w = world().await;
    let id = best_release(&w.books).await;

    let first = w.books.grab(grab(id.clone(), false)).await;
    let again = w.books.grab(grab(id.clone(), false)).await;
    let forced = w.books.grab(grab(id, true)).await;

    assert_eq!(first.outcome, GrabOutcome::Grabbed);
    let hash = hash_of("Project Hail Mary");
    assert_eq!(first.grab_id.as_deref(), Some(hash.as_str()));
    assert_eq!(
        w.deluge.state().adds,
        [(hash.clone(), "books".to_owned(), BOOKS.to_owned())]
    );
    assert_eq!(again.outcome, GrabOutcome::AlreadyHave);
    assert_eq!(
        again.already_have[0].grab_id.as_deref(),
        Some(hash.as_str())
    );
    assert_eq!(forced.outcome, GrabOutcome::AlreadyGrabbed);
    assert_eq!(w.deluge.state().adds.len(), 1);
}

#[tokio::test]
async fn a_label_that_moves_outside_the_library_is_refused() {
    let w = world_with("/elsewhere", json!([]), json!([]), SideEffectMode::Live).await;
    let id = best_release(&w.books).await;

    let summary = w.books.grab(grab(id, false)).await;

    assert_eq!(summary.outcome, GrabOutcome::NotSetUp);
    assert!(w.deluge.state().adds.is_empty());
}

#[tokio::test]
async fn a_book_already_in_the_library_is_not_downloaded_again() {
    let albums =
        json!([{"type": "album", "title": "Project Hail Mary", "parentTitle": "Andy Weir"}]);
    let w = world_with(BOOKS, albums, json!([]), SideEffectMode::Live).await;
    let search = w.books.search(QUERY).await;

    let summary = w
        .books
        .grab(grab(search.releases[0].release_id.clone(), false))
        .await;

    assert_eq!(search.already_have[0].status, "in_library");
    assert_eq!(summary.outcome, GrabOutcome::AlreadyHave);
    assert!(w.deluge.state().adds.is_empty());
}

#[tokio::test]
async fn an_add_with_a_lost_answer_is_verified_by_reading_deluge() {
    let w = world().await;
    w.deluge.state().behavior = Some(AddBehavior::AddButLoseAnswer);
    let id = best_release(&w.books).await;

    let summary = w.books.grab(grab(id, false)).await;

    assert_eq!(summary.outcome, GrabOutcome::Grabbed);
}

#[tokio::test]
async fn an_uncertain_add_is_settled_by_the_watcher_and_never_resent() {
    let w = world().await;
    w.deluge.state().behavior = Some(AddBehavior::LoseAnswer);
    let id = best_release(&w.books).await;
    let hash = hash_of("Project Hail Mary");

    let summary = w.books.grab(grab(id.clone(), true)).await;
    assert_eq!(summary.outcome, GrabOutcome::Uncertain);
    let repeat = w.books.grab(grab(id, true)).await;
    assert_eq!(repeat.outcome, GrabOutcome::AlreadyGrabbed);

    // The add did land after all; the next watch run finds it.
    w.deluge.set(
        &hash,
        TorrentStatus {
            state: "Downloading".to_owned(),
            progress_percent: 5.0,
            save_path: "/incomplete".to_owned(),
            move_completed_path: BOOKS.to_owned(),
            ..TorrentStatus::default()
        },
    );
    w.books.watch(TEST_EPOCH_MS + 60_000).await.expect("watch");

    let status = w.books.status(None).await.expect("status");
    assert_eq!(status.grabs[0].stage, Stage::Downloading);
    assert_eq!(w.deluge.state().adds.len(), 1);
}

#[tokio::test]
async fn a_finished_book_is_scanned_matched_in_plex_and_announced_once() {
    let name = "Project Hail Mary";
    let tracks = json!([{
        "type": "track", "parentRatingKey": "77", "parentTitle": "Project Hail Mary",
        "grandparentTitle": "Andy Weir",
        "Media": [{"Part": [{"file": format!("{BOOKS}/{name}/{name}.m4b")}]}]
    }]);
    let w = world_with(BOOKS, json!([]), tracks, SideEffectMode::Live).await;
    let id = best_release(&w.books).await;
    w.books.grab(grab(id, false)).await;
    let hash = hash_of(name);

    w.books.watch(TEST_EPOCH_MS + 60_000).await.expect("watch");
    assert!(
        w.app.pushes.all().is_empty(),
        "nothing to announce while downloading"
    );

    w.deluge.set(&hash, finished(name));
    let report = w.books.watch(TEST_EPOCH_MS + 120_000).await.expect("watch");
    w.books.watch(TEST_EPOCH_MS + 180_000).await.expect("watch");

    assert_eq!(report.ready, 1);
    let scans: Vec<String> = w
        .plex
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|r| r.url.path() == "/library/sections/4/refresh")
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "path")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(scans, [format!("{BOOKS}/{name}")]);
    let pushes = w.app.pushes.all();
    assert_eq!(pushes.len(), 1, "announced exactly once");
    assert_eq!(pushes[0].message.title.as_deref(), Some("Audiobook ready"));
    assert!(
        pushes[0]
            .message
            .message
            .contains("Project Hail Mary by Andy Weir")
    );
    let status = w.books.status(Some(&hash)).await.expect("status");
    assert_eq!(status.grabs[0].stage, Stage::Ready);
    assert!(status.grabs[0].notified);
}

#[tokio::test]
async fn a_download_plex_never_picks_up_is_flagged_once() {
    let name = "Project Hail Mary";
    let w = world().await;
    let id = best_release(&w.books).await;
    w.books.grab(grab(id, false)).await;
    w.deluge.set(&hash_of(name), finished(name));

    w.books.watch(TEST_EPOCH_MS + 60_000).await.expect("watch");
    w.books
        .watch(TEST_EPOCH_MS + 3 * HOUR_MS)
        .await
        .expect("watch");
    w.books
        .watch(TEST_EPOCH_MS + 4 * HOUR_MS)
        .await
        .expect("watch");

    let status = w.books.status(None).await.expect("status");
    assert_eq!(status.grabs[0].stage, Stage::Finished);
    assert_eq!(status.grabs[0].problem, Some(Problem::NotInPlex));
    let pushes = w.app.pushes.all();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0].message.title.as_deref(),
        Some("Audiobook needs attention")
    );
}

#[tokio::test]
async fn an_unreadable_deluge_degrades_the_run_without_changing_grabs() {
    let w = world().await;
    let id = best_release(&w.books).await;
    w.books.grab(grab(id, false)).await;
    w.deluge.state().unreadable = true;

    let report = w
        .books
        .watch(TEST_EPOCH_MS + 13 * HOUR_MS)
        .await
        .expect("watch");

    assert!(report.degraded.is_some());
    assert!(w.app.pushes.all().is_empty());
}

#[tokio::test]
async fn cancel_removes_only_unfinished_downloads() {
    let name = "Project Hail Mary";
    let w = world().await;
    let id = best_release(&w.books).await;
    w.books.grab(grab(id, false)).await;
    let hash = hash_of(name);

    let cancelled = w.books.cancel(&hash).await;
    assert_eq!(cancelled.outcome, CancelOutcome::Cancelled);
    assert_eq!(w.deluge.state().removes, std::slice::from_ref(&hash));
    let status = w.books.status(None).await.expect("status");
    assert_eq!(status.grabs[0].stage, Stage::Cancelled);

    let mp3 = w.books.search(QUERY).await.releases[1].release_id.clone();
    w.books.grab(grab(mp3, false)).await;
    let mp3_hash = hash_of("PHM mp3");
    w.deluge.set(&mp3_hash, finished("PHM mp3"));
    let refused = w.books.cancel(&mp3_hash).await;
    assert_eq!(refused.outcome, CancelOutcome::Finished);
    assert_eq!(w.deluge.state().removes.len(), 1);
    assert_eq!(
        w.books.cancel(&"0".repeat(40)).await.outcome,
        CancelOutcome::NotFound
    );
}

#[tokio::test]
async fn record_mode_records_instead_of_adding() {
    let w = world_with(BOOKS, json!([]), json!([]), SideEffectMode::Record).await;
    let id = best_release(&w.books).await;
    let id_for_test = id.clone();

    let summary = w.books.grab(grab(id, false)).await;

    assert_eq!(summary.outcome, GrabOutcome::Grabbed);
    assert!(w.deluge.state().adds.is_empty());
    let downloads = w
        ._prowlarr
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|r| r.url.path() == "/3/download")
        .count();
    assert_eq!(downloads, 0, "no tracker snatch in Record mode");
    assert_eq!(
        w.books.recorded(),
        [Recorded::Add {
            release_id: id_for_test,
            label: "books".to_owned()
        }]
    );
}

#[tokio::test]
async fn a_torrent_already_in_deluge_is_left_alone() {
    let w = world().await;
    w.deluge.state().behavior = Some(AddBehavior::AlreadyThere);
    let id = best_release(&w.books).await;

    let summary = w.books.grab(grab(id, false)).await;

    assert_eq!(summary.outcome, GrabOutcome::AlreadyInDeluge);
    let status = w.books.status(None).await.expect("status");
    assert!(status.grabs.is_empty(), "Omni does not track or cancel it");
}

/// MyAnonamouse refuses a VIP torrent to a non-VIP account (Prowlarr passes
/// it on as a bare 500), and Prowlarr's failure backoff then rejects the next
/// search with a 400. Each grab runs exactly one search, so the first two
/// searches (the listing and the first grab) succeed and the third fails.
#[tokio::test]
async fn a_vip_release_is_parsed_and_its_refusals_are_explained() {
    const VIP_QUERY: &str = "Trust the Plan Will Sommer";
    let app = TestApp::new().await;
    let deluge = FakeDeluge::new();
    let prowlarr = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/indexer"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 3, "name": "MyAnonamouse", "enable": true}
        ])))
        .mount(&prowlarr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([mam(
            "Trust the Plan: The Rise of QAnon by Will Sommer [ENG / M4B] [VIP]",
            "77",
            31
        )])))
        .up_to_n_times(2)
        .mount(&prowlarr)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "Search failed due to all selected indexers being unavailable"
        })))
        .mount(&prowlarr)
        .await;
    Mock::given(method("GET"))
        .and(path("/3/download"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&prowlarr)
        .await;
    let plex = plex(BOOKS, json!([]), json!([])).await;
    let books = books(&app, &prowlarr, &plex, &deluge, SideEffectMode::Live);
    let request = |release_id: String| GrabRequest {
        query: VIP_QUERY.to_owned(),
        release_id,
        force: false,
    };

    let search = books.search(VIP_QUERY).await;
    let release = &search.releases[0];
    let refused = books.grab(request(release.release_id.clone())).await;
    let backed_off = books.grab(request(release.release_id.clone())).await;

    assert_eq!(
        release.book.as_deref(),
        Some("Trust the Plan: The Rise of QAnon")
    );
    assert_eq!(release.author.as_deref(), Some("Will Sommer"));
    assert_eq!(release.language.as_deref(), Some("ENG"));
    assert!(!release.good_match);
    assert!(release.notes.iter().any(|n| n.starts_with("VIP torrent")));
    assert_eq!(refused.outcome, GrabOutcome::Unavailable);
    assert_eq!(
        refused.reason.as_deref(),
        Some(
            "the .torrent download answered HTTP 500; this is a VIP torrent, likely \
             refused because MyAnonamouse lets only VIP members download those"
        )
    );
    assert_eq!(backed_off.outcome, GrabOutcome::Unavailable);
    assert_eq!(
        backed_off.reason.as_deref(),
        Some(
            "Prowlarr search answered HTTP 400: Search failed due to all selected \
             indexers being unavailable"
        )
    );
    assert!(deluge.state().adds.is_empty());
}
