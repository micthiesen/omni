//! Local speech: VAD, speaker embeddings, Destiny voiceprint matching,
//! speaker diarization and transcription.
//!
//! [`LocalSpeechRuntime`] holds the decision logic (windowing, scoring,
//! thresholds) over a [`SpeechBackend`]; [`crate::sherpa::SherpaBackend`] is the
//! native backend. The service talks to a [`SpeechEngine`], which runs the
//! blocking native work on tokio's blocking pool.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use omni_runtime::ports::{ChunkTranscript, PortError, SpeechSegment, SpeechTranscriber};
use serde::{Deserialize, Serialize};

use crate::audio::SAMPLE_RATE;

/// Native model and default speaker threshold, identical to the Docker image layout.
pub const DEFAULT_SPEAKER_THRESHOLD: f64 = 0.62;
pub const SPEAKER_WINDOW_SECONDS: usize = 4;
pub const SPEAKER_STRIDE_SECONDS: usize = 2;
pub const TRANSCRIPTION_MODEL: &str = "parakeet-tdt-0.6b-v3-int8";
pub const VOICEPRINT_MODEL: &str = "3dspeaker-campplus-en-voxceleb-16k";

/// A failure inside the local speech pipeline.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{operation}: {cause}")]
pub struct SpeechRecognitionError {
    pub operation: String,
    pub cause: String,
}

impl SpeechRecognitionError {
    pub fn new(operation: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self {
            operation: operation.into(),
            cause: cause.to_string(),
        }
    }
}

/// Best window score and how many windows reached the threshold.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpeakerMatch {
    pub confidence: f64,
    pub matched_windows: u32,
    pub checked_windows: u32,
}

/// Diarized speaker turns shorter than this are dropped: they rarely hold a word.
const MIN_TURN_SECONDS: f64 = 0.6;
/// Consecutive turns of one speaker separated by at most this gap merge.
const TURN_MERGE_GAP_SECONDS: f64 = 1.0;
/// Longer turns are split so each transcription call stays bounded.
const MAX_TURN_SECONDS: f64 = 45.0;
/// Voiceprint windows scored per diarized speaker, longest turns first.
const MAX_SPEAKER_WINDOWS: usize = 12;
/// Voiceprint windows scored per turn of a speaker not already Destiny.
const MAX_TURN_WINDOWS: usize = 6;

/// The seven model paths under `LIVESTREAM_MODEL_DIR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFiles {
    pub vad: PathBuf,
    pub speaker: PathBuf,
    pub segmentation: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
}

impl ModelFiles {
    pub fn in_dir(model_dir: &Path) -> Self {
        let parakeet = model_dir.join("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8");
        Self {
            vad: model_dir.join("silero_vad.int8.onnx"),
            speaker: model_dir.join("3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx"),
            segmentation: model_dir
                .join("sherpa-onnx-pyannote-segmentation-3-0")
                .join("model.onnx"),
            encoder: parakeet.join("encoder.int8.onnx"),
            decoder: parakeet.join("decoder.int8.onnx"),
            joiner: parakeet.join("joiner.int8.onnx"),
            tokens: parakeet.join("tokens.txt"),
        }
    }

    pub fn all(&self) -> [&Path; 7] {
        [
            &self.vad,
            &self.speaker,
            &self.segmentation,
            &self.encoder,
            &self.decoder,
            &self.joiner,
            &self.tokens,
        ]
    }

    /// Every file must exist and be structurally sound before native
    /// initialization, which aborts the process on a bad file
    /// ([`crate::model_check`]).
    pub fn require(&self) -> Result<(), SpeechRecognitionError> {
        for path in self.all() {
            if let Err(cause) = std::fs::metadata(path) {
                return Err(SpeechRecognitionError::new(
                    "validate livestream speech model files",
                    SpeechRecognitionError::new(format!("access {}", path.display()), cause),
                ));
            }
            if let Err(cause) = crate::model_check::check_model_file(path) {
                return Err(SpeechRecognitionError::new(
                    "validate livestream speech model files",
                    SpeechRecognitionError::new(format!("check {}", path.display()), cause),
                ));
            }
        }
        Ok(())
    }
}

/// Voiceprint file format v1 (`LIVESTREAM_DESTINY_VOICEPRINT_PATH`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceprintFile {
    pub version: f64,
    pub speaker: String,
    pub model: String,
    pub embeddings: Vec<Vec<f64>>,
    pub created_at: f64,
    pub sources: Vec<String>,
}

impl VoiceprintFile {
    /// Version 1, speaker "destiny", at least two
    /// non-empty finite embeddings, non-negative `createdAt`.
    pub fn validate(&self) -> Result<(), String> {
        #[allow(clippy::float_cmp)]
        if self.version != 1.0 {
            return Err(format!("version must be 1, got {}", self.version));
        }
        if self.speaker != "destiny" {
            return Err(format!(
                "speaker must be \"destiny\", got {:?}",
                self.speaker
            ));
        }
        if self.embeddings.len() < 2 {
            return Err(format!(
                "embeddings needs at least 2 items, got {}",
                self.embeddings.len()
            ));
        }
        for (index, embedding) in self.embeddings.iter().enumerate() {
            if embedding.is_empty() {
                return Err(format!("embeddings[{index}] is empty"));
            }
            if embedding.iter().any(|v| !v.is_finite()) {
                return Err(format!("embeddings[{index}] has a non-finite value"));
            }
        }
        if self.created_at.is_nan() || self.created_at < 0.0 {
            return Err(format!("createdAt must be >= 0, got {}", self.created_at));
        }
        Ok(())
    }

    /// Reads, parses and validates a voiceprint.
    pub fn load(path: &Path) -> Result<Self, SpeechRecognitionError> {
        let contents = std::fs::read_to_string(path).map_err(|cause| {
            SpeechRecognitionError::new(format!("read voiceprint {}", path.display()), cause)
        })?;
        let value: serde_json::Value = serde_json::from_str(&contents).map_err(|cause| {
            SpeechRecognitionError::new(format!("parse voiceprint {}", path.display()), cause)
        })?;
        let decode = |cause: String| {
            SpeechRecognitionError::new(format!("decode voiceprint {}", path.display()), cause)
        };
        let file: VoiceprintFile =
            serde_json::from_value(value).map_err(|e| decode(e.to_string()))?;
        file.validate().map_err(decode)?;
        Ok(file)
    }

    /// The voiceprint file: `JSON.stringify(voiceprint)` plus a newline.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let value = serde_json::to_value(self)?;
        Ok(format!("{}\n", omni_core::js::json_stringify(&value)))
    }
}

/// Cosine similarity in double precision; `-1` for mismatched or empty vectors.
pub fn cosine_similarity(a: &[f32], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return -1.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (av, bv) in a.iter().zip(b) {
        let av = f64::from(*av);
        dot += av * bv;
        norm_a += av * av;
        norm_b += bv * bv;
    }
    dot / f64::EPSILON.max(norm_a.sqrt() * norm_b.sqrt())
}

/// A stretch of speech found by voice activity detection, as a sample range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechSpan {
    pub start: usize,
    pub end: usize,
}

/// Spans closer than this are transcribed together, so the recognizer sees
/// whole sentences instead of fragments split at a breath.
const SPAN_MERGE_GAP_SECONDS: f64 = 0.8;
/// A merged span stops growing here, which bounds one recognizer call.
const MAX_MERGED_SPAN_SECONDS: usize = 30;
/// A final span ending this close to the end of a chunk is still being spoken.
const OPEN_TAIL_SECONDS: f64 = 0.5;

/// Sorted, in-bounds spans with close neighbours merged
/// ([`SPAN_MERGE_GAP_SECONDS`], [`MAX_MERGED_SPAN_SECONDS`]).
pub fn merge_spans(spans: &[SpeechSpan], total_samples: usize) -> Vec<SpeechSpan> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let gap = (SPAN_MERGE_GAP_SECONDS * f64::from(SAMPLE_RATE)) as usize;
    let max_len = MAX_MERGED_SPAN_SECONDS * SAMPLE_RATE as usize;
    let mut sorted: Vec<SpeechSpan> = spans
        .iter()
        .map(|s| SpeechSpan {
            start: s.start,
            end: s.end.min(total_samples),
        })
        .filter(|s| s.start < s.end)
        .collect();
    sorted.sort_by_key(|s| s.start);
    let mut merged: Vec<SpeechSpan> = Vec::new();
    for span in sorted {
        match merged.last_mut() {
            Some(last)
                if span.start <= last.end + gap
                    && span.end.saturating_sub(last.start) <= max_len =>
            {
                last.end = last.end.max(span.end);
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// The native operations the runtime needs. All calls block.
pub trait SpeechBackend: Send + Sync + 'static {
    /// VAD speech segments of at least one second.
    fn speech_segments(&self, samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError>;
    /// One speaker embedding.
    fn embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError>;
    /// Raw transcript text.
    fn transcribe(&self, samples: &[f32]) -> Result<String, SpeechRecognitionError>;
    /// VAD speech spans of any length, in order.
    fn speech_spans(&self, samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError>;
    /// Speaker-clustered segments, sorted by start time.
    fn diarize(&self, samples: &[f32]) -> Result<Vec<DiarizedSegment>, SpeechRecognitionError>;
}

/// One diarized segment: seconds from the start of the audio and a cluster
/// index that is meaningful only within that audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiarizedSegment {
    pub start: f64,
    pub end: f64,
    pub speaker: u32,
}

/// One speaker's contiguous speech, as a sample range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeakerTurn {
    pub speaker: u32,
    pub start: usize,
    pub end: usize,
}

impl SpeakerTurn {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// A diarized speaker's label and share of the audio.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerProfile {
    pub speaker: u32,
    /// "Destiny" for a voiceprint match, otherwise "Speaker N" by first appearance.
    pub label: String,
    pub seconds: f64,
    /// Best voiceprint score, when a voiceprint is loaded.
    pub destiny_confidence: Option<f64>,
}

/// Speaker turns ready to transcribe, with a profile per speaker.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeakerAttribution {
    pub turns: Vec<SpeakerTurn>,
    pub speakers: Vec<SpeakerProfile>,
}

/// Voiceprint windows scored for one diarized speaker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeakerVoiceMatch {
    pub speaker: u32,
    pub matched_windows: usize,
    pub checked_windows: usize,
    pub confidence: f64,
}

fn seconds_to_samples(seconds: f64, total: usize) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let samples = (seconds.max(0.0) * f64::from(SAMPLE_RATE)).round() as usize;
    samples.min(total)
}

/// Turns from diarized segments: overlapping speech is given to the speaker
/// already talking (so no audio is transcribed twice), turns under
/// [`MIN_TURN_SECONDS`] are dropped, a speaker's turns separated by at most
/// [`TURN_MERGE_GAP_SECONDS`] merge, and turns over [`MAX_TURN_SECONDS`] split.
pub fn speaker_turns(segments: &[DiarizedSegment], total_samples: usize) -> Vec<SpeakerTurn> {
    let rate = SAMPLE_RATE as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let min_len = (MIN_TURN_SECONDS * f64::from(SAMPLE_RATE)) as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let merge_gap = (TURN_MERGE_GAP_SECONDS * f64::from(SAMPLE_RATE)) as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_len = MAX_TURN_SECONDS as usize * rate;
    let mut sorted: Vec<DiarizedSegment> = segments
        .iter()
        .copied()
        .filter(|s| s.start.is_finite() && s.end.is_finite())
        .collect();
    sorted.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut clipped: Vec<SpeakerTurn> = Vec::new();
    let mut covered = 0;
    for segment in sorted {
        let start = seconds_to_samples(segment.start, total_samples).max(covered);
        let end = seconds_to_samples(segment.end, total_samples);
        if end < start + min_len {
            continue;
        }
        covered = end;
        clipped.push(SpeakerTurn {
            speaker: segment.speaker,
            start,
            end,
        });
    }
    let mut merged: Vec<SpeakerTurn> = Vec::new();
    for turn in clipped {
        match merged.last_mut() {
            Some(last) if last.speaker == turn.speaker && turn.start <= last.end + merge_gap => {
                last.end = turn.end;
            }
            _ => merged.push(turn),
        }
    }
    let mut turns = Vec::new();
    for turn in merged {
        let mut start = turn.start;
        while start < turn.end {
            let end = (start + max_len).min(turn.end);
            turns.push(SpeakerTurn {
                speaker: turn.speaker,
                start,
                end,
            });
            start = end;
        }
    }
    turns
}

/// Voiceprint evidence that audio is Destiny: at least two scored windows,
/// at least half of them at the threshold. One strong window is not enough,
/// and a cluster that mostly holds another voice is not Destiny.
pub fn is_destiny_voice(matched_windows: usize, checked_windows: usize) -> bool {
    checked_windows >= 2 && matched_windows * 2 >= checked_windows
}

/// The diarized speakers whose voice is Destiny ([`is_destiny_voice`]).
/// Clustering can split one voice, so several speakers can qualify.
pub fn destiny_speakers(matches: &[SpeakerVoiceMatch]) -> Vec<u32> {
    matches
        .iter()
        .filter(|m| is_destiny_voice(m.matched_windows, m.checked_windows))
        .map(|m| m.speaker)
        .collect()
}

/// Profiles in order of first appearance: the Destiny speaker is "Destiny",
/// the rest are numbered "Speaker 1", "Speaker 2", ...
pub fn label_speakers(
    turns: &[SpeakerTurn],
    destiny: Option<u32>,
    matches: &[SpeakerVoiceMatch],
) -> Vec<SpeakerProfile> {
    let mut profiles: Vec<SpeakerProfile> = Vec::new();
    for turn in turns {
        #[allow(clippy::cast_precision_loss)]
        let seconds = turn.len() as f64 / f64::from(SAMPLE_RATE);
        if let Some(profile) = profiles.iter_mut().find(|p| p.speaker == turn.speaker) {
            profile.seconds += seconds;
            continue;
        }
        let label = if destiny == Some(turn.speaker) {
            "Destiny".to_owned()
        } else {
            let number = profiles
                .iter()
                .filter(|p| Some(p.speaker) != destiny)
                .count()
                + 1;
            format!("Speaker {number}")
        };
        profiles.push(SpeakerProfile {
            speaker: turn.speaker,
            label,
            seconds,
            destiny_confidence: matches
                .iter()
                .find(|m| m.speaker == turn.speaker)
                .map(|m| m.confidence),
        });
    }
    profiles
}

/// `LocalSpeechRuntime`: voiceprint matching and transcription over a backend.
pub struct LocalSpeechRuntime<B> {
    backend: B,
    voiceprint: Option<VoiceprintFile>,
    speaker_threshold: f64,
}

impl<B: SpeechBackend> LocalSpeechRuntime<B> {
    pub fn with_backend(backend: B, voiceprint: Option<VoiceprintFile>, threshold: f64) -> Self {
        Self {
            backend,
            voiceprint,
            speaker_threshold: threshold,
        }
    }

    /// Validates the model files, then loads the voiceprint, then initializes
    /// the backend: a bad voiceprint fails before any native initialization.
    pub fn create(
        model_dir: &Path,
        voiceprint_path: Option<&Path>,
        speaker_threshold: f64,
        init: impl FnOnce(&ModelFiles) -> Result<B, SpeechRecognitionError>,
    ) -> Result<Self, SpeechRecognitionError> {
        let files = ModelFiles::in_dir(model_dir);
        files.require()?;
        let voiceprint = voiceprint_path.map(VoiceprintFile::load).transpose()?;
        let backend = init(&files)?;
        Ok(Self::with_backend(backend, voiceprint, speaker_threshold))
    }

    pub fn has_voiceprint(&self) -> bool {
        self.voiceprint.is_some()
    }

    pub fn extract_speech(&self, samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        self.backend.speech_segments(samples)
    }

    pub fn compute_embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        self.backend.embedding(samples)
    }

    /// Best voiceprint score of each 4 s window (2 s stride) in `segments`,
    /// stopping after `limit` windows.
    fn score_windows<'a>(
        &self,
        voiceprint: &VoiceprintFile,
        segments: impl IntoIterator<Item = &'a [f32]>,
        limit: usize,
    ) -> Result<Vec<f64>, SpeechRecognitionError> {
        let window = SPEAKER_WINDOW_SECONDS * SAMPLE_RATE as usize;
        let stride = SPEAKER_STRIDE_SECONDS * SAMPLE_RATE as usize;
        let mut scores = Vec::new();
        for segment in segments {
            let mut offset = 0;
            while offset + window <= segment.len() {
                if scores.len() >= limit {
                    return Ok(scores);
                }
                let embedding = self.compute_embedding(&segment[offset..offset + window])?;
                let best = voiceprint
                    .embeddings
                    .iter()
                    .map(|reference| cosine_similarity(&embedding, reference))
                    .fold(f64::NEG_INFINITY, crate::js_math::js_max);
                scores.push(best);
                offset += stride;
            }
        }
        Ok(scores)
    }

    /// Scores 4 s windows (2 s stride) of each speech segment against every
    /// enrolled embedding; a window matches at `>= speaker_threshold`.
    pub fn detect_destiny(&self, samples: &[f32]) -> Result<SpeakerMatch, SpeechRecognitionError> {
        let Some(voiceprint) = &self.voiceprint else {
            return Ok(SpeakerMatch::default());
        };
        let speech = self.extract_speech(samples)?;
        let mut scores =
            self.score_windows(voiceprint, speech.iter().map(Vec::as_slice), usize::MAX)?;
        scores.sort_by(|a, b| b.total_cmp(a));
        let matched = scores
            .iter()
            .filter(|score| **score >= self.speaker_threshold)
            .count();
        Ok(SpeakerMatch {
            confidence: scores.first().copied().unwrap_or(0.0),
            matched_windows: u32::try_from(matched).unwrap_or(u32::MAX),
            checked_windows: u32::try_from(scores.len()).unwrap_or(u32::MAX),
        })
    }

    /// Diarizes `samples` into speaker turns and names Destiny: every speaker
    /// whose turns match the voiceprint ([`destiny_speakers`]) becomes one
    /// "Destiny" speaker, as does any other turn that matches by itself.
    pub fn attribute_speakers(
        &self,
        samples: &[f32],
    ) -> Result<SpeakerAttribution, SpeechRecognitionError> {
        let mut turns = speaker_turns(&self.backend.diarize(samples)?, samples.len());
        let mut matches = Vec::new();
        let mut destiny = None;
        if let Some(voiceprint) = &self.voiceprint {
            let mut speakers: Vec<u32> = turns.iter().map(|t| t.speaker).collect();
            speakers.sort_unstable();
            speakers.dedup();
            for speaker in speakers {
                let mut own: Vec<&SpeakerTurn> =
                    turns.iter().filter(|t| t.speaker == speaker).collect();
                own.sort_by_key(|t| std::cmp::Reverse(t.len()));
                let scores = self.score_windows(
                    voiceprint,
                    own.iter().map(|t| &samples[t.start..t.end]),
                    MAX_SPEAKER_WINDOWS,
                )?;
                matches.push(SpeakerVoiceMatch {
                    speaker,
                    matched_windows: scores
                        .iter()
                        .filter(|score| **score >= self.speaker_threshold)
                        .count(),
                    checked_windows: scores.len(),
                    confidence: scores.iter().copied().fold(0.0, crate::js_math::js_max),
                });
            }
            let clusters = destiny_speakers(&matches);
            let canonical = clusters.first().copied().unwrap_or_else(|| {
                turns
                    .iter()
                    .map(|t| t.speaker)
                    .max()
                    .map_or(0, |max| max.saturating_add(1))
            });
            for turn in &mut turns {
                if clusters.contains(&turn.speaker) {
                    turn.speaker = canonical;
                    continue;
                }
                let scores = self.score_windows(
                    voiceprint,
                    [&samples[turn.start..turn.end]],
                    MAX_TURN_WINDOWS,
                )?;
                let matched = scores
                    .iter()
                    .filter(|score| **score >= self.speaker_threshold)
                    .count();
                if is_destiny_voice(matched, scores.len()) {
                    turn.speaker = canonical;
                }
            }
            destiny = turns
                .iter()
                .any(|t| t.speaker == canonical)
                .then_some(canonical);
        }
        let speakers = label_speakers(&turns, destiny, &matches);
        Ok(SpeakerAttribution { turns, speakers })
    }

    /// Trimmed transcript.
    pub fn transcribe(&self, samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        Ok(omni_core::js::trim(&self.backend.transcribe(samples)?).to_owned())
    }

    /// Transcribes each stretch of speech in one chunk of a longer recording.
    /// Unless `last`, an utterance still being spoken at the end of the chunk
    /// is left for the next one: `consumed_samples` is where it starts. A
    /// tail starting in the first half of the chunk is transcribed anyway, so
    /// every chunk advances by at least half its length.
    pub fn transcribe_chunk(
        &self,
        samples: &[f32],
        last: bool,
    ) -> Result<ChunkTranscript, SpeechRecognitionError> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let open_tail = (OPEN_TAIL_SECONDS * f64::from(SAMPLE_RATE)) as usize;
        let mut spans = merge_spans(&self.backend.speech_spans(samples)?, samples.len());
        let mut consumed_samples = samples.len();
        if !last
            && let Some(tail) = spans.last().copied()
            && tail.end + open_tail >= samples.len()
            && tail.start >= samples.len() / 2
        {
            consumed_samples = tail.start;
            spans.pop();
        }
        let mut segments = Vec::new();
        for span in spans {
            let text = self.transcribe(&samples[span.start..span.end])?;
            if !text.is_empty() {
                segments.push(SpeechSegment {
                    start_sample: span.start,
                    end_sample: span.end,
                    text,
                });
            }
        }
        Ok(ChunkTranscript {
            segments,
            consumed_samples,
        })
    }
}

/// The service's speech seam.
pub trait SpeechEngine: Send + Sync + 'static {
    fn has_voiceprint(&self) -> bool;
    fn detect_destiny(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<SpeakerMatch, SpeechRecognitionError>>;
    fn transcribe(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<String, SpeechRecognitionError>>;
    fn attribute_speakers(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<SpeakerAttribution, SpeechRecognitionError>>;
}

/// Runs a [`LocalSpeechRuntime`] on tokio's blocking pool.
pub struct BlockingSpeech<B>(pub Arc<LocalSpeechRuntime<B>>);

impl<B> Clone for BlockingSpeech<B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

async fn blocking<T: Send + 'static>(
    operation: &'static str,
    f: impl FnOnce() -> Result<T, SpeechRecognitionError> + Send + 'static,
) -> Result<T, SpeechRecognitionError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| SpeechRecognitionError::new(operation, e))?
}

impl<B: SpeechBackend> SpeechEngine for BlockingSpeech<B> {
    fn has_voiceprint(&self) -> bool {
        self.0.has_voiceprint()
    }

    fn detect_destiny(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<SpeakerMatch, SpeechRecognitionError>> {
        let runtime = Arc::clone(&self.0);
        Box::pin(blocking("detect speaker", move || {
            runtime.detect_destiny(&samples)
        }))
    }

    fn transcribe(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<String, SpeechRecognitionError>> {
        let runtime = Arc::clone(&self.0);
        Box::pin(blocking("transcribe livestream audio", move || {
            runtime.transcribe(&samples)
        }))
    }

    fn attribute_speakers(
        &self,
        samples: Arc<Vec<f32>>,
    ) -> BoxFuture<'_, Result<SpeakerAttribution, SpeechRecognitionError>> {
        let runtime = Arc::clone(&self.0);
        Box::pin(blocking("diarize livestream audio", move || {
            runtime.attribute_speakers(&samples)
        }))
    }
}

/// The [`SpeechTranscriber`] port over the loaded runtime.
pub struct LocalTranscriber<B>(pub Arc<LocalSpeechRuntime<B>>);

impl<B: SpeechBackend> SpeechTranscriber for LocalTranscriber<B> {
    fn model(&self) -> &'static str {
        TRANSCRIPTION_MODEL
    }

    fn transcribe_chunk(
        &self,
        samples: Arc<Vec<f32>>,
        last: bool,
    ) -> BoxFuture<'_, Result<ChunkTranscript, PortError>> {
        let runtime = Arc::clone(&self.0);
        Box::pin(async move {
            blocking("transcribe audio chunk", move || {
                runtime.transcribe_chunk(&samples, last)
            })
            .await
            .map_err(|error| PortError::Failed {
                message: error.to_string(),
                transient: false,
            })
        })
    }
}
