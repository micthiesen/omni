//! The sherpa-onnx bindings must reproduce the reference speaker path on the
//! same audio, so enrolled voiceprints keep matching at the 0.62 threshold.
//!
//! Ignored by default (needs the Docker image's VAD and speaker models and a
//! reference JSON produced with `sherpa-onnx-node`, which this repository no
//! longer generates). To run:
//!
//! 1. Put `silero_vad.int8.onnx` and
//!    `3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx` (Dockerfile URLs and
//!    SHA-256s) in `MODELS`.
//! 2. Put 16 kHz mono f32le clips `NAME.f32` in `CLIPS`; the first clip is
//!    enrolled, at least one later clip must be the same speaker saying
//!    something else (names starting with the first name), the rest others.
//! 3. `OMNI_SHERPA_MODELS=MODELS OMNI_SHERPA_CLIPS=CLIPS OMNI_SHERPA_REFERENCE=ref.json
//!    cargo test -p omni-live-intel --test sherpa_parity -- --ignored --nocapture`
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use std::collections::HashMap;
use std::path::PathBuf;

use omni_live_intel::audio::decode_f32le;
use omni_live_intel::sherpa::SherpaSpeaker;
use omni_live_intel::speech::{
    DEFAULT_SPEAKER_THRESHOLD, DiarizedSegment, LocalSpeechRuntime, SpeechBackend,
    SpeechRecognitionError, SpeechSpan, VoiceprintFile, cosine_similarity,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detect {
    confidence: f64,
    matched_windows: u32,
    checked_windows: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Clip {
    segment_lengths: Vec<usize>,
    windows: Vec<Vec<f64>>,
    detect: Detect,
}

#[derive(Deserialize)]
struct Reference {
    names: Vec<String>,
    voiceprint: VoiceprintFile,
    clips: HashMap<String, Clip>,
}

/// The speaker half of the native backend (the spike needs no recognizer).
struct Speaker(SherpaSpeaker);

impl SpeechBackend for Speaker {
    fn speech_segments(&self, samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        self.0.speech_segments(samples)
    }

    fn embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        self.0.embedding(samples)
    }

    fn speech_spans(&self, samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        self.0.speech_spans(samples)
    }

    fn transcribe(&self, _samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        Err(SpeechRecognitionError::new(
            "transcribe livestream audio",
            "not loaded for the speaker spike",
        ))
    }

    fn diarize(&self, _samples: &[f32]) -> Result<Vec<DiarizedSegment>, SpeechRecognitionError> {
        Err(SpeechRecognitionError::new(
            "diarize livestream audio",
            "not loaded for the speaker spike",
        ))
    }
}

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

#[test]
#[ignore = "needs the speaker models and a sherpa-onnx-node reference fixture"]
fn rust_bindings_reproduce_the_node_speaker_path() {
    let models = env_path("OMNI_SHERPA_MODELS");
    let clips = env_path("OMNI_SHERPA_CLIPS");
    let reference: Reference = serde_json::from_slice(
        &std::fs::read(env_path("OMNI_SHERPA_REFERENCE")).expect("reference"),
    )
    .expect("reference json");
    reference.voiceprint.validate().expect("valid voiceprint");
    let speaker = SherpaSpeaker::new(
        &models.join("silero_vad.int8.onnx"),
        &models.join("3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx"),
    )
    .expect("speaker models");
    let runtime = LocalSpeechRuntime::with_backend(
        Speaker(speaker),
        Some(reference.voiceprint.clone()),
        DEFAULT_SPEAKER_THRESHOLD,
    );
    let enrolled = &reference.names[0];
    let mut same_speaker_checked = 0;
    for name in &reference.names {
        let clip = &reference.clips[name];
        let bytes = std::fs::read(clips.join(format!("{name}.f32"))).expect("clip");
        let samples = decode_f32le(&bytes);
        let segments = runtime.extract_speech(&samples).expect("vad");
        let lengths: Vec<usize> = segments.iter().map(Vec::len).collect();
        assert_eq!(lengths, clip.segment_lengths, "{name}: VAD segments");

        let mut windows = Vec::new();
        for segment in &segments {
            let mut offset = 0;
            while offset + 64_000 <= segment.len() {
                windows.push(
                    runtime
                        .compute_embedding(&segment[offset..offset + 64_000])
                        .expect("embedding"),
                );
                offset += 32_000;
            }
        }
        assert_eq!(windows.len(), clip.windows.len(), "{name}: window count");
        let agreement = windows
            .iter()
            .zip(&clip.windows)
            .map(|(rust, node)| cosine_similarity(rust, node))
            .fold(f64::INFINITY, f64::min);
        let max_delta = windows
            .iter()
            .zip(&clip.windows)
            .flat_map(|(rust, node)| {
                rust.iter()
                    .zip(node)
                    .map(|(r, n)| (f64::from(*r) - n).abs())
            })
            .fold(0.0, f64::max);

        let detected = runtime.detect_destiny(&samples).expect("detect");
        println!(
            "{name}: segments={} windows={} min_cos(rust,node)={agreement:.9} max_abs_delta={max_delta:.3e} \
             rust={:.6}/{}/{} node={:.6}/{}/{}",
            lengths.len(),
            windows.len(),
            detected.confidence,
            detected.matched_windows,
            detected.checked_windows,
            clip.detect.confidence,
            clip.detect.matched_windows,
            clip.detect.checked_windows,
        );
        assert!(
            agreement > 0.9999,
            "{name}: embeddings diverge ({agreement})"
        );
        assert!(
            (detected.confidence - clip.detect.confidence).abs() < 1e-4,
            "{name}: confidence"
        );
        assert_eq!(
            detected.matched_windows, clip.detect.matched_windows,
            "{name}"
        );
        assert_eq!(
            detected.checked_windows, clip.detect.checked_windows,
            "{name}"
        );
        if name != enrolled && name.starts_with(enrolled.as_str()) {
            same_speaker_checked += 1;
            assert!(
                detected.confidence >= DEFAULT_SPEAKER_THRESHOLD,
                "{name}: the enrolled speaker must match at >= 0.62"
            );
        } else if !name.starts_with(enrolled.as_str()) {
            assert!(
                detected.confidence < DEFAULT_SPEAKER_THRESHOLD,
                "{name}: another speaker must not match"
            );
        }
    }
    assert!(
        same_speaker_checked > 0,
        "needs a second clip of the enrolled speaker"
    );
}
