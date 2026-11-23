//! Chunk verification, adaptive re-splitting and checkpoint resume over a
//! fake verified provider, an echo STT and a recording fake ffmpeg.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use common::{EchoStt, FakeTts};
use omni_ai::costs::CostRecorder;
use omni_presspods::costs::CostCounter;
use omni_presspods::speech::audio_chain::AudioChain;
use omni_presspods::speech::stt::SttClient;
use omni_presspods::speech::synthesize::{Synthesizer, render_signature};
use omni_presspods::storage::{AudioStore, checkpoint_key};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

struct Setup {
    synthesizer: Synthesizer,
    _store: TestStore,
    dir: tempfile::TempDir,
}

async fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let ffmpeg = common::fake_ffmpeg(dir.path(), &dir.path().join("ffmpeg.log"));
    let ffprobe = common::fake_ffprobe(dir.path(), "2.5");
    std::fs::write(dir.path().join("intro.mp3"), b"INTRO").unwrap();
    let clock: omni_core::clock::SharedClock = test_clock(TEST_EPOCH_MS);
    let store = TestStore::new(clock.clone()).await;
    let synthesizer = Synthesizer {
        chain: AudioChain::new(
            ffmpeg.display().to_string(),
            ffprobe.display().to_string(),
            Path::new("assets/press-pods/denoise.rnnn"),
            tmp,
        )
        .unwrap(),
        storage: AudioStore::new(dir.path().join("audio")),
        costs: CostCounter::new(),
        recorder: CostRecorder::new(store.store.clone(), clock.clone()),
        clock,
        intro_path: dir.path().join("intro.mp3"),
    };
    Setup {
        synthesizer,
        _store: store,
        dir,
    }
}

/// One paragraph of six ~100-char sentences: a single initial chunk (under
/// the 650-char verified profile) that re-splits at the 400-char level.
fn paragraph() -> String {
    (0..6)
        .map(|i| {
            let words: Vec<String> = (0..14).map(|j| format!("t{i}x{j}")).collect();
            format!("Sentence {}.", words.join(" "))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test]
async fn verified_takes_are_checkpointed_and_resumed_without_synthesis() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::verified());
    let stt: Arc<dyn SttClient> = Arc::new(EchoStt::default());
    let content = paragraph();
    let first = s
        .synthesizer
        .synthesize_speech(tts.clone(), Some(stt.clone()), &content, Some("work"))
        .await
        .unwrap();
    assert_eq!(tts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.chunks.len(), 1);
    assert_eq!(first.chunks[0].coverage, Some(1.0));
    assert_eq!(first.chunks[0].attempts, 1);
    assert_eq!(first.voice_provider, "FakeHiggs");
    let key = checkpoint_key(&render_signature(tts.as_ref()), &content);
    assert!(
        s.synthesizer
            .storage
            .read_chunk_checkpoint("work", &key)
            .await
            .is_some()
    );

    let resumed = s
        .synthesizer
        .synthesize_speech(tts.clone(), Some(stt), &content, Some("work"))
        .await
        .unwrap();
    assert_eq!(
        tts.calls.load(Ordering::SeqCst),
        1,
        "resume must not re-synthesize"
    );
    assert_eq!(resumed.chunks[0].attempts, 0);
}

#[tokio::test]
async fn a_truncated_splittable_chunk_is_resplit_into_verified_pieces() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::verified());
    let stt = Arc::new(EchoStt::default());
    stt.truncate_first.store(1, Ordering::SeqCst);
    let result = s
        .synthesizer
        .synthesize_speech(
            tts.clone(),
            Some(stt.clone() as Arc<dyn SttClient>),
            &paragraph(),
            Some("work"),
        )
        .await
        .unwrap();
    // One probe of the full chunk, then each sub-chunk.
    assert!(result.chunks.len() > 1);
    assert_eq!(tts.calls.load(Ordering::SeqCst), 1 + result.chunks.len());
    for chunk in &result.chunks {
        assert_eq!(chunk.resplit, Some(true));
        assert_eq!(chunk.resplit_depth, Some(1));
        assert_eq!(chunk.coverage, Some(1.0));
    }
    let joined: Vec<&str> = result.chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        joined.join(" ").split_whitespace().count(),
        paragraph().split_whitespace().count()
    );
    // Chunk gaps (0.7 s) separate the re-split pieces; offsets include the 2.5 s intro.
    assert_eq!(result.chunks[0].start_time_seconds, 2.5);
    assert_eq!(result.chunks[1].start_time_seconds, 2.5 + 2.5 + 0.7);
}

#[tokio::test]
async fn an_unsplittable_chunk_keeps_the_best_take_after_three_attempts() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::verified());
    let stt = Arc::new(EchoStt::default());
    stt.truncate_first.store(10, Ordering::SeqCst);
    let words: Vec<String> = (0..40).map(|j| format!("word{j}")).collect();
    let sentence = format!("{}.", words.join(" "));
    let result = s
        .synthesizer
        .synthesize_speech(
            tts.clone(),
            Some(stt as Arc<dyn SttClient>),
            &sentence,
            Some("work"),
        )
        .await
        .unwrap();
    assert_eq!(tts.calls.load(Ordering::SeqCst), 3);
    assert_eq!(result.chunks.len(), 1);
    assert_eq!(result.chunks[0].attempts, 3);
    // A failing take is never checkpointed.
    let key = checkpoint_key(&render_signature(tts.as_ref()), &sentence);
    assert!(
        s.synthesizer
            .storage
            .read_chunk_checkpoint("work", &key)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn an_stt_outage_ships_without_resplitting_or_caching() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::verified());
    let stt = Arc::new(EchoStt {
        fail: true,
        ..EchoStt::default()
    });
    let content = paragraph();
    let result = s
        .synthesizer
        .synthesize_speech(
            tts.clone(),
            Some(stt as Arc<dyn SttClient>),
            &content,
            Some("work"),
        )
        .await
        .unwrap();
    assert_eq!(tts.calls.load(Ordering::SeqCst), 1);
    assert_eq!(result.chunks.len(), 1);
    assert_eq!(result.chunks[0].coverage, None);
    let key = checkpoint_key(&render_signature(tts.as_ref()), &content);
    assert!(
        s.synthesizer
            .storage
            .read_chunk_checkpoint("work", &key)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn short_chunks_skip_verification() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::verified());
    let stt = Arc::new(EchoStt::default());
    let result = s
        .synthesizer
        .synthesize_speech(
            tts.clone(),
            Some(stt.clone() as Arc<dyn SttClient>),
            "Too short to verify.",
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.chunks[0].attempts, 1);
    assert_eq!(stt.calls.load(Ordering::SeqCst), 0);
    assert!(
        s.dir.path().join("audio").read_dir().is_err(),
        "no checkpoint without a work id"
    );
}

#[tokio::test]
async fn sections_become_chapters_after_the_intro() {
    let s = setup().await;
    let tts = Arc::new(FakeTts::clean());
    let result = s
        .synthesizer
        .synthesize_speech(
            tts,
            None,
            "Hook.\n\n## One\n\nFirst.\n\n## Two\n\nSecond.",
            None,
        )
        .await
        .unwrap();
    let chapters: Vec<(f64, &str)> = result
        .chapters
        .iter()
        .map(|c| (c.start_time_seconds, c.title.as_str()))
        .collect();
    assert_eq!(
        chapters,
        [
            (2.5, "Introduction"),
            (2.5 + 2.5 + 1.5, "One"),
            (2.5 + 5.0 + 3.0, "Two")
        ]
    );
    assert_eq!(result.chunks[1].section_title.as_deref(), Some("One"));
    assert_eq!(result.audio, b"FAKE-AUDIO");
}
