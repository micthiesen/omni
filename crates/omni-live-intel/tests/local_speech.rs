//! Local speech: the speaker windowing and threshold logic over a fake backend (the native sherpa models
//! are not available to tests).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::Mutex;

use omni_live_intel::load_speech_runtime;
use omni_live_intel::speech::{
    DEFAULT_SPEAKER_THRESHOLD, DiarizedSegment, LocalSpeechRuntime, ModelFiles, SpeakerTurn,
    SpeakerVoiceMatch, SpeechBackend, SpeechRecognitionError, SpeechSpan, VoiceprintFile,
    cosine_similarity, destiny_speakers, is_destiny_voice, label_speakers, speaker_turns,
};

fn speech_fixture(voiceprint: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let files = ModelFiles::in_dir(dir.path());
    for path in files.all() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        // Structurally valid stand-ins: they pass the pre-native file
        // checks, and every test here fails before native initialization.
        let contents: &[u8] = if path.extension().is_some_and(|e| e == "txt") {
            b"<unk> 0\n"
        } else {
            &[0x08, 0x08, 0x3a, 0x01, 0x00]
        };
        std::fs::write(path, contents).expect("write model fixture");
    }
    let voiceprint_path = dir.path().join("destiny.json");
    std::fs::write(&voiceprint_path, voiceprint).expect("write voiceprint");
    (dir, voiceprint_path)
}

#[test]
fn returns_a_typed_failure_for_malformed_voiceprint_json() {
    let (dir, voiceprint) = speech_fixture("{not-json");
    let error = load_speech_runtime(dir.path(), Some(&voiceprint), DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert!(error.operation.contains("parse voiceprint"), "{error}");
}

#[test]
fn rejects_a_structurally_invalid_voiceprint_before_native_initialization() {
    let (dir, voiceprint) = speech_fixture(
        r#"{"version":1,"speaker":"destiny","model":"test","embeddings":[[0.1,0.2]],"createdAt":1,"sources":["test"]}"#,
    );
    let error = load_speech_runtime(dir.path(), Some(&voiceprint), DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert!(error.operation.contains("decode voiceprint"), "{error}");
}

#[test]
fn empty_or_corrupt_model_files_fail_validation_before_native_initialization() {
    let (dir, _) = speech_fixture("");
    let files = ModelFiles::in_dir(dir.path());
    std::fs::write(&files.encoder, "").expect("truncate encoder");
    let backend_called = std::cell::Cell::new(false);
    let error = LocalSpeechRuntime::<FakeBackend>::create(
        dir.path(),
        None,
        DEFAULT_SPEAKER_THRESHOLD,
        |_| {
            backend_called.set(true);
            Err(SpeechRecognitionError::new("unreachable", "native init"))
        },
    )
    .err()
    .expect("fails");
    assert_eq!(error.operation, "validate livestream speech model files");
    assert!(error.cause.contains("encoder.int8.onnx"), "{error}");
    assert!(error.cause.contains("file is empty"), "{error}");
    assert!(!backend_called.get());

    std::fs::write(&files.encoder, "<html>gateway timeout</html>").expect("corrupt encoder");
    let error = load_speech_runtime(dir.path(), None, DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert!(error.cause.contains("not a valid ONNX model"), "{error}");

    std::fs::write(&files.encoder, [0x08, 0x08]).expect("graphless encoder");
    let error = load_speech_runtime(dir.path(), None, DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert!(error.cause.contains("no graph"), "{error}");

    std::fs::write(&files.encoder, [0x08, 0x08, 0x3a, 0x01, 0x00]).expect("fix encoder");
    std::fs::write(&files.tokens, "").expect("empty tokens");
    let error = load_speech_runtime(dir.path(), None, DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert!(error.cause.contains("tokens.txt"), "{error}");
}

#[test]
fn missing_model_files_fail_validation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = load_speech_runtime(dir.path(), None, DEFAULT_SPEAKER_THRESHOLD)
        .err()
        .expect("fails");
    assert_eq!(error.operation, "validate livestream speech model files");
}

/// Segments and diarization are given; each window's embedding is
/// `[first sample, 1]`.
struct FakeBackend {
    segments: Vec<Vec<f32>>,
    windows: Mutex<Vec<usize>>,
    diarized: Vec<DiarizedSegment>,
}

impl SpeechBackend for FakeBackend {
    fn speech_segments(&self, _samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        Ok(self.segments.clone())
    }

    fn embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        self.windows.lock().expect("lock").push(samples.len());
        Ok(vec![samples[0], 1.0])
    }

    fn speech_spans(&self, _samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        Ok(vec![])
    }

    fn transcribe(&self, _samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        Ok("  hello there  ".to_owned())
    }

    fn diarize(&self, _samples: &[f32]) -> Result<Vec<DiarizedSegment>, SpeechRecognitionError> {
        Ok(self.diarized.clone())
    }
}

fn backend(segments: Vec<Vec<f32>>) -> FakeBackend {
    FakeBackend {
        segments,
        windows: Mutex::new(vec![]),
        diarized: vec![],
    }
}

fn segment(start: f64, end: f64, speaker: u32) -> DiarizedSegment {
    DiarizedSegment {
        start,
        end,
        speaker,
    }
}

fn turn(speaker: u32, start_seconds: f64, end_seconds: f64) -> SpeakerTurn {
    SpeakerTurn {
        speaker,
        start: (start_seconds * 16_000.0) as usize,
        end: (end_seconds * 16_000.0) as usize,
    }
}

fn voiceprint(embeddings: Vec<Vec<f64>>) -> VoiceprintFile {
    VoiceprintFile {
        version: 1.0,
        speaker: "destiny".into(),
        model: "3dspeaker-campplus-en-voxceleb-16k".into(),
        embeddings,
        created_at: 1.0,
        sources: vec![],
    }
}

#[test]
fn scores_four_second_windows_with_a_two_second_stride() {
    // A 9 s segment yields windows at 0, 2, 4 s (3 windows); a 3 s one yields none.
    let mut long = vec![0.0_f32; 9 * 16_000];
    long[0] = 1.0; // window 0 -> [1, 1]
    long[2 * 16_000] = -1.0; // window 1 -> [-1, 1]
    long[4 * 16_000] = 0.0; // window 2 -> [0, 1]
    let backend = backend(vec![long, vec![0.0; 3 * 16_000]]);
    let runtime = LocalSpeechRuntime::with_backend(
        backend,
        Some(voiceprint(vec![vec![1.0, 1.0], vec![0.0, -1.0]])),
        DEFAULT_SPEAKER_THRESHOLD,
    );
    let matched = runtime.detect_destiny(&[]).expect("detects");
    assert_eq!(matched.checked_windows, 3);
    // Scores: 1.0, 0.0 (best of 0 and -1), 0.7071 -> two at or above 0.62.
    assert_eq!(matched.matched_windows, 2);
    assert!((matched.confidence - 1.0).abs() < 1e-9);
    assert_eq!(runtime.transcribe(&[]).expect("text"), "hello there");
}

#[test]
fn without_a_voiceprint_nothing_is_checked() {
    let backend = backend(vec![vec![0.0; 5 * 16_000]]);
    let runtime = LocalSpeechRuntime::with_backend(backend, None, DEFAULT_SPEAKER_THRESHOLD);
    let matched = runtime.detect_destiny(&[]).expect("detects");
    assert_eq!(matched.checked_windows, 0);
    assert_eq!(matched.confidence, 0.0);
    assert!(!runtime.has_voiceprint());
}

#[test]
fn cosine_similarity_edge_cases() {
    assert_eq!(cosine_similarity(&[], &[]), -1.0);
    assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), -1.0);
    assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-12);
    assert_eq!(cosine_similarity(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
}

#[test]
fn voiceprint_files_round_trip_in_the_stored_format() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("destiny.json");
    let file = voiceprint(vec![vec![0.25, -0.5], vec![1.0, 0.000_001]]);
    std::fs::write(&path, file.to_json_line().expect("json")).expect("write");
    let text = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        text,
        "{\"version\":1,\"speaker\":\"destiny\",\"model\":\"3dspeaker-campplus-en-voxceleb-16k\",\"embeddings\":[[0.25,-0.5],[1,0.000001]],\"createdAt\":1,\"sources\":[]}\n"
    );
    assert_eq!(VoiceprintFile::load(Path::new(&path)).expect("loads"), file);
}

#[test]
fn speaker_turns_give_overlapping_speech_to_the_speaker_already_talking() {
    let turns = speaker_turns(
        &[
            segment(0.0, 5.0, 0),
            segment(4.0, 9.0, 1),
            segment(4.5, 4.8, 0),
        ],
        10 * 16_000,
    );
    assert_eq!(turns, vec![turn(0, 0.0, 5.0), turn(1, 5.0, 9.0)]);
}

#[test]
fn speaker_turns_drop_blips_and_merge_a_speaker_across_short_gaps() {
    let turns = speaker_turns(
        &[
            segment(0.0, 3.0, 0),
            segment(3.2, 3.5, 1),
            segment(3.6, 6.0, 0),
            segment(8.0, 9.0, 0),
        ],
        10 * 16_000,
    );
    // The 0.3 s blip is dropped, a 0.6 s gap merges, a 2 s gap does not.
    assert_eq!(turns, vec![turn(0, 0.0, 6.0), turn(0, 8.0, 9.0)]);
}

#[test]
fn speaker_turns_split_long_turns_and_clamp_to_the_audio() {
    let turns = speaker_turns(&[segment(0.0, 100.0, 2)], 95 * 16_000);
    assert_eq!(
        turns,
        vec![turn(2, 0.0, 45.0), turn(2, 45.0, 90.0), turn(2, 90.0, 95.0)]
    );
}

fn voice_match(speaker: u32, matched: usize, checked: usize, confidence: f64) -> SpeakerVoiceMatch {
    SpeakerVoiceMatch {
        speaker,
        matched_windows: matched,
        checked_windows: checked,
        confidence,
    }
}

#[test]
fn destiny_is_every_speaker_whose_windows_mostly_match() {
    // Clustering can split one voice: both halves are Destiny.
    assert_eq!(
        destiny_speakers(&[
            voice_match(0, 3, 12, 0.9),
            voice_match(1, 9, 12, 0.8),
            voice_match(2, 6, 12, 0.85),
        ]),
        vec![1, 2]
    );
    assert!(destiny_speakers(&[]).is_empty());
    // A single strong window, or a minority of matches, is not enough.
    assert!(!is_destiny_voice(1, 1));
    assert!(!is_destiny_voice(5, 12));
    assert!(is_destiny_voice(2, 2));
}

#[test]
fn speakers_are_labeled_in_order_of_first_appearance() {
    let turns = [
        turn(3, 0.0, 2.0),
        turn(1, 2.0, 6.0),
        turn(3, 6.0, 7.0),
        turn(0, 7.0, 8.0),
    ];
    let profiles = label_speakers(&turns, Some(1), &[voice_match(1, 2, 2, 0.7)]);
    let labels: Vec<(u32, &str, f64)> = profiles
        .iter()
        .map(|p| (p.speaker, p.label.as_str(), p.seconds))
        .collect();
    assert_eq!(
        labels,
        vec![
            (3, "Speaker 1", 3.0),
            (1, "Destiny", 4.0),
            (0, "Speaker 2", 1.0)
        ]
    );
    assert_eq!(profiles[1].destiny_confidence, Some(0.7));
    assert_eq!(profiles[0].destiny_confidence, None);
}

#[test]
fn attribution_names_the_speaker_matching_the_voiceprint() {
    // Speaker 0 talks for 0-8 s, speaker 1 for 8-16 s; embeddings are
    // `[first sample, 1]`, so -1 scores 0 and 1 scores 1 against `[1, 1]`.
    let mut samples = vec![-1.0_f32; 16 * 16_000];
    for window in 0..3 {
        samples[(8 + 2 * window) * 16_000] = 1.0;
    }
    let mut fake = backend(vec![]);
    fake.diarized = vec![segment(0.0, 8.0, 0), segment(8.0, 16.0, 1)];
    let runtime = LocalSpeechRuntime::with_backend(
        fake,
        Some(voiceprint(vec![vec![1.0, 1.0], vec![1.0, 1.0]])),
        DEFAULT_SPEAKER_THRESHOLD,
    );
    let attribution = runtime.attribute_speakers(&samples).expect("attributes");
    assert_eq!(
        attribution.turns,
        vec![turn(0, 0.0, 8.0), turn(1, 8.0, 16.0)]
    );
    let labels: Vec<&str> = attribution
        .speakers
        .iter()
        .map(|p| p.label.as_str())
        .collect();
    assert_eq!(labels, vec!["Speaker 1", "Destiny"]);
}

#[test]
fn attribution_without_a_voiceprint_numbers_every_speaker() {
    let mut fake = backend(vec![]);
    fake.diarized = vec![segment(0.0, 8.0, 0), segment(8.0, 16.0, 1)];
    let runtime = LocalSpeechRuntime::with_backend(fake, None, DEFAULT_SPEAKER_THRESHOLD);
    let attribution = runtime
        .attribute_speakers(&vec![0.0; 16 * 16_000])
        .expect("attributes");
    let labels: Vec<&str> = attribution
        .speakers
        .iter()
        .map(|p| p.label.as_str())
        .collect();
    assert_eq!(labels, vec!["Speaker 1", "Speaker 2"]);
    assert!(
        attribution
            .speakers
            .iter()
            .all(|p| p.destiny_confidence.is_none())
    );
}

#[test]
fn a_matching_turn_inside_another_speaker_is_attributed_to_destiny() {
    // Speaker 0 holds 0-10 s and 12-18 s; only 12-18 s matches the voiceprint:
    // two of six windows over the whole speaker, both windows of that turn.
    let mut samples = vec![-1.0_f32; 18 * 16_000];
    samples[12 * 16_000] = 1.0;
    samples[14 * 16_000] = 1.0;
    let mut fake = backend(vec![]);
    fake.diarized = vec![
        segment(0.0, 10.0, 0),
        segment(10.0, 12.0, 1),
        segment(12.0, 18.0, 0),
    ];
    let runtime = LocalSpeechRuntime::with_backend(
        fake,
        Some(voiceprint(vec![vec![1.0, 1.0], vec![1.0, 1.0]])),
        DEFAULT_SPEAKER_THRESHOLD,
    );
    let attribution = runtime.attribute_speakers(&samples).expect("attributes");
    let labels: Vec<(&str, f64)> = attribution
        .speakers
        .iter()
        .map(|p| (p.label.as_str(), p.seconds))
        .collect();
    assert_eq!(
        labels,
        vec![("Speaker 1", 10.0), ("Speaker 2", 2.0), ("Destiny", 6.0)]
    );
}
