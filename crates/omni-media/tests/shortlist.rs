//! The candidate shortlist, plus a scripted-model
//! shortlist run.
#![allow(clippy::expect_used)]

use crate::common;

use omni_ai::{GenerateResponse, ModelRole};
use omni_media::shortlist::{compute_composite, format_candidate_details, shortlist_candidates};
use omni_media::types::{Candidate, CandidateSource, MediaType};

#[test]
fn weights_taste_match_most_heavily() {
    assert!(compute_composite(90.0, 50.0, 50.0, 1.0) > compute_composite(50.0, 90.0, 50.0, 1.0));
}

#[test]
fn shrinks_scores_toward_the_middle_at_low_confidence() {
    let confident = compute_composite(80.0, 80.0, 80.0, 1.0);
    let unsure = compute_composite(80.0, 80.0, 80.0, 0.0);
    assert!((unsure - confident / 2.0).abs() < f64::EPSILON);
}

#[test]
fn is_bounded_by_0_and_100() {
    assert!(compute_composite(100.0, 100.0, 100.0, 1.0) <= 100.0);
    assert!(compute_composite(0.0, 0.0, 0.0, 0.0).abs() < f64::EPSILON);
}

fn show() -> Candidate {
    Candidate {
        canonical_id: "tmdb:tv:123".to_owned(),
        tmdb_id: 123,
        media_type: MediaType::Tv,
        title: "A Show".to_owned(),
        genres: vec!["Drama".to_owned()],
        vote_average: 8.0,
        vote_count: 1000.0,
        popularity: 10.0,
        source: CandidateSource::Similar,
        in_library: false,
        runtime_minutes: Some(52.0),
        season_count: Some(4.0),
        episode_count: Some(40.0),
        series_status: Some("Ended".to_owned()),
        certification: Some("TV-MA".to_owned()),
        creators: Some(vec!["A Creator".to_owned()]),
        cast: Some(vec!["One".to_owned(), "Two".to_owned()]),
        keywords: Some(vec!["mystery".to_owned(), "workplace".to_owned()]),
        ..Candidate::default()
    }
}

#[test]
fn exposes_viewing_commitment_and_useful_structured_context_to_the_models() {
    let details = format_candidate_details(&show(), true);
    assert!(details.contains("52 min/episode"));
    assert!(details.contains("4 seasons"));
    assert!(details.contains("40 episodes"));
    assert!(details.contains("creator=A Creator"));
    assert!(details.contains("themes=mystery, workplace"));
    assert!(!format_candidate_details(&show(), false).contains("creator="));
}

#[tokio::test]
async fn ranks_scored_candidates_and_skips_unknown_ids() {
    let h = common::Harness::new().await;
    let mut movie = show();
    movie.canonical_id = "tmdb:movie:7".to_owned();
    movie.tmdb_id = 7;
    movie.media_type = MediaType::Movie;
    movie.in_library = true;
    h.app.ai.script(
        ModelRole::RecsShortlist,
        vec![GenerateResponse::text(
            serde_json::json!({"scores": [
                {"candidate_id": "tmdb:tv:123", "taste_match": 50, "novelty": 50, "effort_fit": 50, "confidence": 1, "risks": []},
                {"candidate_id": "tmdb:movie:7", "taste_match": 90, "novelty": 70, "effort_fit": 80, "confidence": 0.5, "risks": ["long"]},
                {"candidate_id": "tmdb:movie:999", "taste_match": 99, "novelty": 99, "effort_fit": 99, "confidence": 1, "risks": []}
            ]})
            .to_string(),
        )],
    );
    let model = h
        .services
        .ai
        .model_for(&h.services.config, ModelRole::RecsShortlist)
        .expect("model");
    let finalists = shortlist_candidates(
        &h.services.ai,
        model.as_ref(),
        &[show(), movie],
        "history",
        None,
        5,
    )
    .await
    .expect("shortlist");
    assert_eq!(
        finalists
            .iter()
            .map(|f| f.candidate.canonical_id.as_str())
            .collect::<Vec<_>>(),
        vec!["tmdb:movie:7", "tmdb:tv:123"]
    );
    let requests = h.app.ai.requests();
    let prompt = format!("{:?}", requests[0].1.messages);
    assert!(prompt.contains("IN LOCAL LIBRARY"));
    assert!(prompt.contains(
        "[tmdb:movie:7] A Show [movie] Drama | rating 8.0 (1000 votes) | source=similar"
    ));
}
