//! The PressPods pipeline, with end-to-end runs over in-process fakes
//! (retrievers, models, TTS, ffmpeg).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeRetriever, HarnessOptions, article, harness, metadata_json};
use omni_ai::{GenerateResponse, ModelRole};
use omni_presspods::error::PressPodsError;
use omni_presspods::model::RetrieverAttempt;

#[tokio::test]
async fn removes_the_final_mp3_when_persistence_fails() {
    let h = harness(HarnessOptions::default()).await;
    let episode = common::episode("https://example.com/article", 1);
    let result = h
        .service
        .persist_episode_with(&episode, b"audio", async {
            Err(PressPodsError::failed(
                "persist test episode",
                "database unavailable",
            ))
        })
        .await;
    assert!(result.is_err());
    let path = h
        .service
        .audio()
        .episode_audio_path(&episode.audio_file)
        .unwrap();
    assert!(!path.exists());
    // Guard against a path change making the assertion inspect another name.
    assert_eq!(
        path.file_name().unwrap().to_str(),
        Some(episode.audio_file.as_str())
    );
}

#[tokio::test]
async fn keeps_file_and_row_together_on_success() {
    let h = harness(HarnessOptions::default()).await;
    let episode = common::episode("https://example.com/article", 1);
    h.service
        .persist_episode_with_audio(&episode, b"audio")
        .await
        .unwrap();
    let path = h
        .service
        .audio()
        .episode_audio_path(&episode.audio_file)
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"audio");
    assert!(
        h.service
            .persistence()
            .get_episode(&episode.episode_id)
            .await
            .unwrap()
            .is_some()
    );
}

fn long_text() -> String {
    "The article body explains the situation in plenty of detail for narration. ".repeat(6)
}

#[tokio::test]
async fn creates_an_episode_from_the_best_rated_retriever() {
    let h = harness(HarnessOptions {
        retrievers: vec![
            Arc::new(FakeRetriever {
                name: "readability",
                result: Ok(article(&long_text(), "same")),
            }),
            Arc::new(FakeRetriever {
                name: "postlight",
                result: Ok(article(&long_text(), "same")),
            }),
            Arc::new(FakeRetriever {
                name: "fetch",
                result: Err("HTTP 500".into()),
            }),
        ],
        ..HarnessOptions::default()
    })
    .await;
    // Identical extractions are rated once and the verdict fans out.
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(true, 9.0))],
    );
    h.app.ai.script(
        ModelRole::PressPodsCleaning,
        vec![
            GenerateResponse::text("no tags here"),
            GenerateResponse::text("<cleaned_article>Hook.\n\n## Part One\n\nBody one.\n\n## Part Two\n\nBody two.</cleaned_article>"),
        ],
    );
    let old = common::episode("https://example.com/story?utm_source=x", 0);
    h.service
        .persist_episode_with_audio(&old, b"old")
        .await
        .unwrap();

    let episode = h
        .service
        .create_episode_from_url("https://example.com/story", Some("PressPods:run".into()))
        .await
        .unwrap();

    assert_eq!(episode.title, "Rated title");
    assert_eq!(episode.author.as_deref(), Some("Jane Writer"));
    assert_eq!(episode.publication.as_deref(), Some("Example"));
    assert_eq!(episode.retriever_name.as_deref(), Some("readability"));
    assert_eq!(
        episode.content,
        "Hook.\n\n## Part One\n\nBody one.\n\n## Part Two\n\nBody two."
    );
    assert_eq!(episode.voice_provider.as_deref(), Some("FakeClean"));
    assert_eq!(
        episode.normalized_url.as_deref(),
        Some("https://example.com/story")
    );
    assert_eq!(episode.run_id.as_deref(), Some("PressPods:run"));
    let chapters = episode.chapters.clone().unwrap();
    let titles: Vec<&str> = chapters.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Introduction", "Part One", "Part Two"]);
    // intro 2.5 s, then 2.5 s per chunk with a 1.5 s section gap between sections.
    assert_eq!(chapters[1].start_time_seconds, 2.5 + 2.5 + 1.5);
    assert_eq!(episode.chunks.as_ref().unwrap().len(), 3);
    let attempts = episode.retriever_attempts.clone().unwrap();
    assert!(
        matches!(&attempts[1], RetrieverAttempt::Success { name, content_rating, .. } if name == "postlight" && *content_rating == 9.0)
    );
    assert!(
        matches!(&attempts[2], RetrieverAttempt::Failure { name, error, .. } if name == "fetch" && error == "retrieve article with fetch: HTTP 500")
    );
    let costs = episode.costs.clone().unwrap();
    assert!(costs.detail_tokens.keys().any(|k| k.ends_with("-meta")));
    assert!(costs.detail_chars.contains_key("fake-tts-tts"));

    // The file landed, the older take for the same article was replaced, and
    // the listener was told.
    let audio = h
        .service
        .audio()
        .episode_audio_path(&episode.audio_file)
        .unwrap();
    assert!(audio.exists());
    let rows = h.service.persistence().get_all_episodes().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        !h.service
            .audio()
            .episode_audio_path(&old.audio_file)
            .unwrap()
            .exists()
    );
    let pushes = h.app.pushes.all();
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        pushes[0].message.title.as_deref(),
        Some("Episode Now Available")
    );
    assert!(
        pushes[0]
            .message
            .message
            .starts_with("'Rated title' from 'example.com' is now available.\n")
    );
    assert_eq!(
        pushes[0].message.url.as_deref(),
        Some("http://omni.boris/pods")
    );

    // Every ffmpeg filter graph kept the HQ resampler; no denoise for a clean provider.
    let calls = common::ffmpeg_calls(&h.ffmpeg_log);
    assert!(!calls.is_empty());
    for call in &calls {
        for arg in call {
            for (i, _) in arg.match_indices("aresample=") {
                let rest = &arg[i..];
                assert!(rest.contains("filter_size=256:cutoff=0.95"), "{arg}");
            }
            assert!(!arg.contains("arnndn"));
        }
    }
}

#[tokio::test]
async fn fails_when_every_retriever_fails_or_is_invalid() {
    let h = harness(HarnessOptions {
        retrievers: vec![
            Arc::new(FakeRetriever {
                name: "readability",
                result: Ok(article(&long_text(), "invalid")),
            }),
            Arc::new(FakeRetriever {
                name: "fetch",
                result: Err("HTTP 403".into()),
            }),
        ],
        ..HarnessOptions::default()
    })
    .await;
    h.app.ai.script(
        ModelRole::PressPodsMetadata,
        vec![GenerateResponse::text(metadata_json(false, 2.0))],
    );
    let error = h
        .service
        .create_episode_from_url("https://example.com/story", None)
        .await
        .unwrap_err();
    assert_eq!(
        error.cause_message(),
        "All article retrievers failed (readability: Invalid article; fetch: HTTP 403)"
    );
    assert!(!error.is_retryable());
}
