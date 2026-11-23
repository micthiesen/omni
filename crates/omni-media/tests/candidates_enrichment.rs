//! Candidate enrichment with TMDB details.
#![allow(clippy::expect_used)]

use crate::common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omni_media::candidates::{PooledCandidate, enrich_candidates};
use omni_media::tmdb::types::TmdbTitleDetails;
use omni_media::types::{CandidateSource, MediaType, make_canonical_id};

fn candidate(tmdb_id: i64) -> PooledCandidate {
    PooledCandidate {
        canonical_id: make_canonical_id(MediaType::Movie, tmdb_id),
        tmdb_id,
        media_type: MediaType::Movie,
        title: format!("Movie {tmdb_id}"),
        genre_ids: vec![18],
        vote_average: 7.0,
        vote_count: 500.0,
        popularity: 5.0,
        source: CandidateSource::Similar,
        ..PooledCandidate::default()
    }
}

#[tokio::test]
async fn keeps_a_candidate_when_its_detail_request_fails() {
    let logs = omni_testkit::capture_logs();
    let catalog = common::FakeCatalog::default();
    catalog
        .genre_names
        .lock()
        .expect("lock")
        .insert(18, "Drama".to_owned());
    catalog.set_details(Arc::new(|_, tmdb_id| {
        Box::pin(async move {
            if tmdb_id == 1 {
                Ok(TmdbTitleDetails {
                    runtime_minutes: Some(110.0),
                    origin_countries: vec!["US".to_owned()],
                    ..TmdbTitleDetails::default()
                })
            } else {
                Err(common::failure("TMDB GET /movie/2", "TMDB unavailable"))
            }
        })
    }));
    let library: HashSet<String> = HashSet::from(["tmdb:movie:1".to_owned()]);
    let enriched = enrich_candidates(&catalog, &[candidate(1), candidate(2)], &library)
        .await
        .expect("enrich");
    assert_eq!(enriched.len(), 2);
    assert_eq!(enriched[0].runtime_minutes, Some(110.0));
    assert_eq!(enriched[0].genres, vec!["Drama".to_owned()]);
    assert!(enriched[0].in_library);
    assert_eq!(enriched[1].genres, vec!["Drama".to_owned()]);
    assert!(!enriched[1].in_library);
    assert!(logs.events().iter().any(|e| {
        e.level == tracing::Level::WARN
            && e.message
                .contains("TMDB details fetch failed for tmdb:movie:2")
            && e.message.contains("TMDB unavailable")
    }));
}

#[tokio::test]
async fn bounds_concurrent_detail_requests() {
    let catalog = common::FakeCatalog::default();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (release, released) = tokio::sync::watch::channel(false);
    let release = Arc::new(release);
    {
        let (active, maximum, release) = (active.clone(), maximum.clone(), release.clone());
        catalog.set_details(Arc::new(move |_, _| {
            let (active, maximum, release) = (active.clone(), maximum.clone(), release.clone());
            let mut released = released.clone();
            Box::pin(async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                if now == 6 {
                    let _ = release.send(true);
                }
                let _ = released.wait_for(|open| *open).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(TmdbTitleDetails::default())
            })
        }));
    }
    let pool: Vec<PooledCandidate> = (1..=12).map(candidate).collect();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        enrich_candidates(&catalog, &pool, &HashSet::new()),
    )
    .await
    .expect("completes")
    .expect("enrich");
    let maximum = maximum.load(Ordering::SeqCst);
    assert!(maximum <= 6);
    assert!(maximum > 1);
}
