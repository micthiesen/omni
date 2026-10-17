//! Podcast Index authentication headers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_podcasts::podcastindex::{
    PodcastIndexCredentials, podcast_index_auth_hash, podcast_index_auth_headers,
};

fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.as_str())
        .unwrap()
}

#[test]
fn is_deterministic_for_the_same_inputs() {
    assert_eq!(
        podcast_index_auth_hash("my-key", "my-secret", "1234567890"),
        podcast_index_auth_hash("my-key", "my-secret", "1234567890")
    );
}

#[test]
fn returns_a_40_character_lowercase_hex_sha1_digest() {
    let hash = podcast_index_auth_hash("my-key", "my-secret", "1234567890");
    assert_eq!(hash.len(), 40);
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
}

#[test]
fn changes_when_any_input_changes() {
    let base = podcast_index_auth_hash("my-key", "my-secret", "1234567890");
    assert_ne!(
        podcast_index_auth_hash("other-key", "my-secret", "1234567890"),
        base
    );
    assert_ne!(
        podcast_index_auth_hash("my-key", "other-secret", "1234567890"),
        base
    );
    assert_ne!(
        podcast_index_auth_hash("my-key", "my-secret", "1234567891"),
        base
    );
}

#[test]
fn returns_all_four_required_headers() {
    let now_ms = 1_700_000_000_000;
    let headers = podcast_index_auth_headers(
        &PodcastIndexCredentials {
            key: "my-key".to_owned(),
            secret: "my-secret".to_owned(),
        },
        now_ms,
    );
    assert_eq!(header(&headers, "X-Auth-Key"), "my-key");
    assert_eq!(header(&headers, "X-Auth-Date"), "1700000000");
    assert_eq!(
        header(&headers, "Authorization"),
        podcast_index_auth_hash("my-key", "my-secret", "1700000000")
    );
    assert_eq!(header(&headers, "User-Agent"), "omni/1.0");
}

#[test]
fn computes_x_auth_date_as_floor_now_ms_over_1000() {
    let headers = podcast_index_auth_headers(
        &PodcastIndexCredentials {
            key: "k".to_owned(),
            secret: "s".to_owned(),
        },
        1_700_000_000_999,
    );
    assert_eq!(header(&headers, "X-Auth-Date"), "1700000000");
}
