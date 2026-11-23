//! Media identity resolution, plus store-backed
//! resolution cases for the alias cache.
#![allow(clippy::expect_used)]

use crate::common;

use omni_media::identity::{
    SearchResult, parse_guid_external_ids, resolve_identity, score_search_results,
};
use omni_media::persistence::{IdentityAliasData, ResolutionPath};
use omni_media::types::{ExternalIds, MediaType};
use omni_store::EntityOps as _;

fn ids(tmdb: Option<i64>, imdb: Option<&str>, tvdb: Option<i64>) -> ExternalIds {
    ExternalIds {
        tmdb,
        imdb: imdb.map(str::to_owned),
        tvdb,
    }
}

#[test]
fn parses_plain_tmdb_guids() {
    assert_eq!(
        parse_guid_external_ids("tmdb://12345"),
        ids(Some(12345), None, None)
    );
}

#[test]
fn parses_plain_imdb_guids() {
    assert_eq!(
        parse_guid_external_ids("imdb://tt0111161"),
        ids(None, Some("tt0111161"), None)
    );
}

#[test]
fn parses_plain_tvdb_guids() {
    assert_eq!(
        parse_guid_external_ids("tvdb://81189"),
        ids(None, None, Some(81189))
    );
}

#[test]
fn parses_legacy_agent_guids_with_query_strings() {
    assert_eq!(
        parse_guid_external_ids("com.plexapp.agents.imdb://tt0111161?lang=en"),
        ids(None, Some("tt0111161"), None)
    );
    assert_eq!(
        parse_guid_external_ids("com.plexapp.agents.themoviedb://603?lang=en"),
        ids(Some(603), None, None)
    );
    assert_eq!(
        parse_guid_external_ids("com.plexapp.agents.thetvdb://81189/3/7?lang=en"),
        ids(None, None, Some(81189))
    );
}

#[test]
fn returns_empty_for_opaque_guids() {
    assert_eq!(
        parse_guid_external_ids("plex://movie/5d7768ba96b655001fdc0408"),
        ExternalIds::default()
    );
    assert_eq!(
        parse_guid_external_ids("local://12345"),
        ExternalIds::default()
    );
}

fn hit(tmdb_id: i64, title: &str, year: Option<i64>, votes: f64) -> SearchResult {
    SearchResult {
        tmdb_id,
        title: title.to_owned(),
        year,
        vote_count: votes,
    }
}

#[test]
fn accepts_a_single_title_year_match_with_high_confidence() {
    let result = score_search_results(
        "The Thing",
        Some(1982),
        MediaType::Movie,
        &[
            hit(1091, "The Thing", Some(1982), 8000.0),
            hit(60935, "The Thing", Some(2011), 3000.0),
        ],
    )
    .expect("resolution");
    assert_eq!(result.canonical_id.as_deref(), Some("tmdb:movie:1091"));
    assert!(result.confidence >= 0.95);
}

#[test]
fn tolerates_off_by_one_release_years() {
    let result = score_search_results(
        "The Thing",
        Some(1982),
        MediaType::Movie,
        &[hit(1091, "The Thing", Some(1981), 8000.0)],
    )
    .expect("resolution");
    assert_eq!(result.canonical_id.as_deref(), Some("tmdb:movie:1091"));
}

#[test]
fn returns_unresolved_for_ambiguous_matches_with_similar_vote_counts() {
    let result = score_search_results(
        "The Thing",
        None,
        MediaType::Movie,
        &[
            hit(1091, "The Thing", Some(1982), 8000.0),
            hit(60935, "The Thing", Some(2011), 3000.0),
        ],
    )
    .expect("resolution");
    assert_eq!(result.canonical_id, None);
}

#[test]
fn accepts_a_dominant_match_when_one_result_dwarfs_the_rest() {
    let result = score_search_results(
        "The Thing",
        None,
        MediaType::Movie,
        &[
            hit(1091, "The Thing", Some(1982), 50000.0),
            hit(99999, "The Thing", Some(2005), 12.0),
        ],
    )
    .expect("resolution");
    assert_eq!(result.canonical_id.as_deref(), Some("tmdb:movie:1091"));
}

#[test]
fn normalizes_punctuation_and_accents_in_titles() {
    let result = score_search_results(
        "Amelie",
        Some(2001),
        MediaType::Movie,
        &[hit(194, "Amélie", Some(2001), 11000.0)],
    )
    .expect("resolution");
    assert_eq!(result.canonical_id.as_deref(), Some("tmdb:movie:194"));
}

#[test]
fn returns_undefined_when_nothing_matches_the_title() {
    let result = score_search_results(
        "The Thing",
        Some(1982),
        MediaType::Movie,
        &[hit(1, "Something Else", Some(1982), 100.0)],
    );
    assert!(result.is_none());
}

#[tokio::test]
async fn caches_network_failures_but_not_offline_misses() {
    let h = common::Harness::new().await;
    let item = common::media(
        "plex://movie/opaque",
        "Unknown Film",
        MediaType::Movie,
        None,
    );
    let offline = resolve_identity(&h.services.store, &h.catalog, &item, false)
        .await
        .expect("offline resolution");
    assert_eq!(offline.resolution_path, ResolutionPath::Unresolved);
    let cached = h
        .services
        .store
        .read(|docs| docs.get::<IdentityAliasData>(&"plex://movie/opaque".to_owned()))
        .await
        .expect("read");
    assert!(cached.is_none(), "offline misses stay uncached");

    let networked = resolve_identity(&h.services.store, &h.catalog, &item, true)
        .await
        .expect("network resolution");
    assert_eq!(networked.canonical_id, None);
    let cached = h
        .services
        .store
        .read(|docs| docs.get::<IdentityAliasData>(&"plex://movie/opaque".to_owned()))
        .await
        .expect("read")
        .expect("failure cached");
    assert_eq!(cached.canonical_id, None);
    assert_eq!(cached.title, "Unknown Film");
}

#[tokio::test]
async fn resolves_unique_tmdb_find_matches() {
    let h = common::Harness::new().await;
    h.catalog.find.lock().expect("lock").insert(
        "tt0111161".to_owned(),
        Ok(vec![
            common::tmdb_title(278, MediaType::Movie),
            common::tmdb_title(9, MediaType::Tv),
        ]),
    );
    let item = common::media("imdb://tt0111161", "Shawshank", MediaType::Movie, None);
    let resolution = resolve_identity(&h.services.store, &h.catalog, &item, true)
        .await
        .expect("resolution");
    assert_eq!(resolution.canonical_id.as_deref(), Some("tmdb:movie:278"));
    assert_eq!(resolution.resolution_path, ResolutionPath::TmdbFind);
    assert!((resolution.confidence - 0.98).abs() < f64::EPSILON);
}
