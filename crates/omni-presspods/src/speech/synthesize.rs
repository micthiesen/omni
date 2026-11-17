//! Speech synthesis with verification, adaptive re-splitting and resumable
//! checkpoints.
//!
//! Each chunk is synthesized, prepared by the audio chain and verified before
//! it is accepted: an STT round trip (word coverage) is the primary verifier
//! for Higgs, the seconds-per-char band the fallback. The best take is kept
//! even if none passes. A splittable chunk that keeps failing is re-split on
//! sentence boundaries (Higgs truncation is length-correlated). Every
//! verified take is checkpointed so a restart resumes from the last good
//! chunk. Temporary WAVs are owned values, so every failure path cleans up.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use omni_ai::costs::CostRecorder;
use omni_core::clock::SharedClock;
use omni_core::js::utf16_len;

use super::audio_chain::{AudioChain, PreparedChunk, SPEED_MULTIPLIER, TempFile};
use super::coverage::{CoverageResult, compute_coverage, is_content_complete};
use super::providers::TtsProvider;
use super::stt::SttClient;
use super::text_chunking::{chunk_text, split_chunk_for_retry, split_sections};
use crate::costs::CostCounter;
use crate::error::PressPodsError;
use crate::model::{Chapter, ChunkStat};
use crate::storage::{AudioStore, checkpoint_key};

const LOG: &str = "PressPods";

/// Fallback verifier band (seconds of trimmed, sped audio per input char).
pub const MIN_SEC_PER_CHAR: f64 = 0.03 / SPEED_MULTIPLIER;
pub const MAX_SEC_PER_CHAR: f64 = 0.15 / SPEED_MULTIPLIER;
/// Ranks fallback takes only when every attempt is out of bounds.
pub const IDEAL_SEC_PER_CHAR: f64 = 0.06 / SPEED_MULTIPLIER;
pub const MAX_SYNTH_ATTEMPTS: u32 = 3;
/// Below this the verifiers are dominated by fixed overhead; checks are skipped.
pub const MIN_VERIFY_CHARS: usize = 120;
/// Full-size attempts a splittable chunk gets before it is re-split.
pub const RESPLIT_PROBE_ATTEMPTS: u32 = 1;

/// Gaps between chunks (paragraph-ish) and between sections.
pub const CHUNK_GAP_SEC: f64 = 0.7;
pub const SECTION_GAP_SEC: f64 = 1.5;

/// Initial chunk sizing: Higgs failures rise sharply around 700 chars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkProfile {
    pub target: usize,
    pub max: usize,
}

pub const CONTENT_VERIFIED_CHUNK_PROFILE: ChunkProfile = ChunkProfile {
    target: 650,
    max: 700,
};
pub const DEFAULT_CHUNK_PROFILE: ChunkProfile = ChunkProfile {
    target: 900,
    max: 1500,
};

/// Everything about the render that changes the produced audio bytes; folded
/// into checkpoint keys so a voice, provider or speed change never reuses a
/// stale take.
pub fn render_signature(provider: &dyn TtsProvider) -> String {
    [
        provider.provider_name().to_owned(),
        provider.voice_name().to_owned(),
        provider.model_id().to_owned(),
        if provider.needs_denoise() {
            "dn"
        } else {
            "raw"
        }
        .to_owned(),
        format!("x{}", omni_core::js::number_to_string(SPEED_MULTIPLIER)),
    ]
    .join("|")
}

/// A verifier's read on one take. `score` only compares takes of the same kind.
#[derive(Clone, Debug)]
struct Assessment {
    accept: bool,
    /// A real STT content check (vs the duration fallback).
    verified: bool,
    score: f64,
    coverage: Option<CoverageResult>,
    description: String,
}

#[allow(clippy::cast_precision_loss)]
fn duration_assessment(take: &PreparedChunk, text: &str) -> Assessment {
    let chars = utf16_len(text) as f64;
    let ratio = take.duration_seconds / chars;
    Assessment {
        accept: (MIN_SEC_PER_CHAR..=MAX_SEC_PER_CHAR).contains(&ratio),
        verified: false,
        score: -(ratio - IDEAL_SEC_PER_CHAR).abs(),
        coverage: None,
        description: format!(
            "{:.1}s for {} chars",
            take.duration_seconds,
            utf16_len(text)
        ),
    }
}

/// Synthesis outcome for one chunk.
struct ChunkOutcome {
    chunk: PreparedChunk,
    /// Takes spent (0 when resumed from a checkpoint).
    attempts: u32,
    coverage: Option<CoverageResult>,
    /// The chosen take cleared verification (or verification was unavailable).
    passed: bool,
}

/// One synthesized, verified, concat-ready piece of narration.
#[derive(Debug)]
pub struct ChunkPiece {
    pub chunk: PreparedChunk,
    pub text: String,
    pub attempts: u32,
    pub coverage: Option<CoverageResult>,
    pub resplit: bool,
    pub resplit_depth: Option<u32>,
}

/// Per-article checkpoint context.
#[derive(Clone, Debug)]
pub struct Checkpoints {
    pub work_id: String,
    pub signature: String,
}

/// What synthesis needs besides the provider.
pub struct Synthesizer {
    pub chain: AudioChain,
    pub storage: AudioStore,
    pub costs: CostCounter,
    pub recorder: CostRecorder,
    pub clock: SharedClock,
    pub intro_path: PathBuf,
}

/// The assembled episode audio and its diagnostics.
#[derive(Debug)]
pub struct SynthesisResult {
    pub audio: Vec<u8>,
    pub voice_name: String,
    pub voice_provider: String,
    pub synthesized_seconds: f64,
    pub chapters: Vec<Chapter>,
    pub chunks: Vec<ChunkStat>,
}

enum Segment {
    ChunkGap,
    SectionGap,
    Piece(usize),
}

impl Synthesizer {
    async fn assess(
        &self,
        take: &PreparedChunk,
        raw: &[u8],
        text: &str,
        stt: Option<&dyn SttClient>,
        use_content: bool,
    ) -> Assessment {
        let Some(stt) = stt.filter(|_| use_content) else {
            return duration_assessment(take, text);
        };
        match stt.transcribe(raw).await {
            Ok(transcript) => {
                let coverage = compute_coverage(text, &transcript);
                Assessment {
                    accept: is_content_complete(&coverage),
                    verified: true,
                    // Penalize runaway (ratio > 1) as much as truncation.
                    score: coverage.coverage - (coverage.word_ratio - 1.0).max(0.0),
                    coverage: Some(coverage),
                    description: format!(
                        "coverage={:.0}% ratio={:.2}",
                        coverage.coverage * 100.0,
                        coverage.word_ratio
                    ),
                }
            }
            Err(error) => {
                tracing::warn!(target: LOG, "STT verify failed ({error}); falling back to duration check");
                duration_assessment(take, text)
            }
        }
    }

    async fn synth(
        &self,
        provider: &dyn TtsProvider,
        text: &str,
    ) -> Result<(PreparedChunk, Vec<u8>), PressPodsError> {
        let raw = provider.synthesize_chunk(text).await?;
        // Every real TTS response is billed, retried and re-split takes included.
        self.costs
            .record_tts_usage(&self.recorder, provider.model_id(), "tts", text)
            .await;
        let chunk = self
            .chain
            .prepare_chunk(&raw, provider.needs_denoise())
            .await?;
        Ok((chunk, raw))
    }

    /// Best-effort: caches a verified take's WAV for resume.
    async fn cache(&self, ckpt: Option<&Checkpoints>, key: Option<&str>, chunk: &PreparedChunk) {
        let (Some(ckpt), Some(key)) = (ckpt, key) else {
            return;
        };
        if let Ok(wav) = tokio::fs::read(chunk.wav.path()).await {
            self.storage
                .write_chunk_checkpoint(&ckpt.work_id, key, &wav)
                .await;
        }
    }

    async fn synthesize_chunk_audio(
        &self,
        provider: &dyn TtsProvider,
        text: &str,
        stt: Option<&dyn SttClient>,
        ckpt: Option<&Checkpoints>,
        max_attempts: u32,
    ) -> Result<ChunkOutcome, PressPodsError> {
        let key = ckpt.map(|c| checkpoint_key(&c.signature, text));
        if let (Some(ckpt), Some(key)) = (ckpt, key.as_deref())
            && let Some(cached) = self.storage.read_chunk_checkpoint(&ckpt.work_id, key).await
        {
            match self.chain.resume_chunk(&cached).await {
                Ok(chunk) => {
                    tracing::info!(target: LOG, "Resumed chunk from checkpoint ({} chars)", utf16_len(text));
                    return Ok(ChunkOutcome {
                        chunk,
                        attempts: 0,
                        coverage: None,
                        passed: true,
                    });
                }
                Err(error) => {
                    self.storage
                        .delete_chunk_checkpoint(&ckpt.work_id, key)
                        .await;
                    tracing::warn!(target: LOG, "Discarded unreadable chunk checkpoint: {error}");
                }
            }
        }

        let use_content = provider.verify_chunk_content() && stt.is_some();
        let verify = provider.verify_chunk_length() || use_content;
        if !verify || utf16_len(text) < MIN_VERIFY_CHARS {
            let (chunk, _) = self.synth(provider, text).await?;
            self.cache(ckpt, key.as_deref(), &chunk).await;
            return Ok(ChunkOutcome {
                chunk,
                attempts: 1,
                coverage: None,
                passed: true,
            });
        }

        let mut takes: Vec<(PreparedChunk, Assessment)> = Vec::new();
        let mut attempts_made = 0;
        let mut last_error: Option<PressPodsError> = None;
        let mut retryable_error: Option<PressPodsError> = None;
        for i in 1..=max_attempts {
            attempts_made = i;
            match self.synth(provider, text).await {
                Ok((chunk, raw)) => {
                    let assessment = self.assess(&chunk, &raw, text, stt, use_content).await;
                    let done = assessment.accept && (assessment.verified || !use_content);
                    let description = assessment.description.clone();
                    takes.push((chunk, assessment));
                    if done {
                        break;
                    }
                    if i < max_attempts {
                        tracing::warn!(target: LOG, "Chunk verify failed ({description}); retry {i}/{max_attempts}");
                    }
                }
                Err(error) => {
                    tracing::warn!(target: LOG, "Chunk synth/prepare failed (attempt {i}/{max_attempts}): {error}");
                    // Another take cannot reach a server that is not there. A
                    // timeout may be one slow take, so it keeps its retries.
                    if error.is_speech_connect_failure() {
                        return Err(error);
                    }
                    if retryable_error.is_none() && error.is_retryable() {
                        retryable_error = Some(error);
                    } else {
                        last_error = Some(error);
                    }
                }
            }
        }

        if takes.is_empty() {
            // Keep the provider error's identity so the job queue can tell a
            // transient outage (retry later) from a permanent failure.
            return Err(retryable_error.or(last_error).unwrap_or_else(|| {
                PressPodsError::failed(
                    "synthesize chunk",
                    format!("All {max_attempts} synthesis attempts failed for a chunk"),
                )
            }));
        }

        // When content verification was intended and some take got a real STT
        // read, choose only among those: scores of different kinds never compare.
        let any_verified = takes.iter().any(|(_, a)| a.verified);
        let in_pool = |a: &Assessment| !(use_content && any_verified) || a.verified;
        let chosen_index = takes
            .iter()
            .position(|(_, a)| in_pool(a) && a.accept)
            .or_else(|| {
                takes
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, a))| in_pool(a))
                    .fold(None::<(usize, f64)>, |best, (i, (_, a))| match best {
                        Some((_, score)) if score >= a.score => best,
                        _ => Some((i, a.score)),
                    })
                    .map(|(i, _)| i)
            })
            .unwrap_or(0);
        let (chunk, assessment) = takes.swap_remove(chosen_index);
        // Discarded takes' temporary WAVs are removed here.
        drop(takes);

        let verification_unavailable = use_content && !any_verified;
        let passed = verification_unavailable || assessment.accept;
        if verification_unavailable {
            tracing::warn!(
                target: LOG,
                "Content verification unavailable for every take (STT failing); shipping the duration-best take ({}) — truncation may slip through",
                assessment.description
            );
        } else if !assessment.accept {
            tracing::warn!(
                target: LOG,
                "Chunk still failing verification after {attempts_made} tries ({})",
                assessment.description
            );
        }
        // Only a genuinely validated take is cached: a resume skips
        // verification, so an STT-outage "pass" must never be locked in.
        let truly_verified = if use_content {
            assessment.verified && assessment.accept
        } else {
            assessment.accept
        };
        if truly_verified {
            self.cache(ckpt, key.as_deref(), &chunk).await;
        }
        Ok(ChunkOutcome {
            chunk,
            attempts: attempts_made,
            coverage: assessment.coverage,
            passed,
        })
    }

    /// Synthesizes one chunk, re-splitting it on sentence boundaries when it
    /// keeps failing verification. Transient provider failures propagate (the
    /// durable job retry owns them) instead of fanning out across a re-split.
    pub fn synthesize_chunk_adaptive<'a>(
        &'a self,
        provider: &'a dyn TtsProvider,
        text: &'a str,
        stt: Option<&'a dyn SttClient>,
        ckpt: Option<&'a Checkpoints>,
        depth: usize,
    ) -> BoxFuture<'a, Result<Vec<ChunkPiece>, PressPodsError>> {
        Box::pin(async move {
            let sub_chunks = split_chunk_for_retry(text, depth);
            let splittable = sub_chunks.is_some();
            let budget = if splittable {
                RESPLIT_PROBE_ATTEMPTS
            } else {
                MAX_SYNTH_ATTEMPTS
            };
            let outcome = match self
                .synthesize_chunk_audio(provider, text, stt, ckpt, budget)
                .await
            {
                Ok(outcome) => Some(outcome),
                Err(error) => {
                    if !splittable || error.is_retryable() {
                        return Err(error);
                    }
                    tracing::warn!(target: LOG, "Chunk synthesis threw on every probe ({error}); re-splitting to recover");
                    None
                }
            };
            if let Some(outcome) = outcome
                && (outcome.passed || !splittable)
            {
                return Ok(vec![ChunkPiece {
                    chunk: outcome.chunk,
                    text: text.to_owned(),
                    attempts: outcome.attempts,
                    coverage: outcome.coverage,
                    resplit: false,
                    resplit_depth: None,
                }]);
            }
            let Some(sub_chunks) = sub_chunks else {
                return Err(PressPodsError::failed(
                    "split failing synthesis chunk",
                    "Missing adaptive chunk split",
                ));
            };
            tracing::warn!(
                target: LOG,
                "Re-splitting failing chunk at level {} ({} chars) into {} boundary-safe sub-chunks and re-synthesizing",
                depth + 1,
                utf16_len(text),
                sub_chunks.len()
            );
            let level = u32::try_from(depth + 1).unwrap_or(u32::MAX);
            let mut pieces = Vec::new();
            for sub in &sub_chunks {
                let sub_pieces = self
                    .synthesize_chunk_adaptive(provider, sub, stt, ckpt, depth + 1)
                    .await?;
                for mut piece in sub_pieces {
                    piece.resplit = true;
                    piece.resplit_depth = Some(piece.resplit_depth.unwrap_or(0).max(level));
                    pieces.push(piece);
                }
            }
            Ok(pieces)
        })
    }

    /// Synthesizes the whole narration into the finished episode audio.
    #[allow(clippy::too_many_lines)]
    pub async fn synthesize_speech(
        &self,
        provider: Arc<dyn TtsProvider>,
        stt: Option<Arc<dyn SttClient>>,
        content: &str,
        work_id: Option<&str>,
    ) -> Result<SynthesisResult, PressPodsError> {
        let start = self.clock.now_ms();
        let provider = provider.as_ref();
        let stt = stt.as_deref().filter(|_| provider.verify_chunk_content());
        let ckpt = work_id.map(|work_id| Checkpoints {
            work_id: work_id.to_owned(),
            signature: render_signature(provider),
        });
        let profile = if provider.verify_chunk_content() {
            CONTENT_VERIFIED_CHUNK_PROFILE
        } else {
            DEFAULT_CHUNK_PROFILE
        };
        let sections = split_sections(content);
        tracing::info!(
            target: LOG,
            provider = provider.provider_name(),
            voice = provider.voice_name(),
            model = provider.model_id(),
            total_chars = utf16_len(content),
            sections = sections.len(),
            content_verify = stt.map(|s| s.model_id()).unwrap_or("off"),
            chunk_target = profile.target,
            chunk_max = profile.max,
            "Starting speech synthesis"
        );
        if provider.verify_chunk_content() && stt.is_none() {
            tracing::warn!(
                target: LOG,
                "Content verification unavailable (no PRESSPODS_STT_URL / PRESSPODS_TTS_URL); falling back to the duration-band check, which lets some truncation through"
            );
        }

        let intro = tokio::fs::read(&self.intro_path)
            .await
            .map_err(|e| PressPodsError::io("read PressPods intro", e))?;
        let intro_duration = self
            .chain
            .probe_duration_seconds(&self.intro_path)
            .await
            .map_err(|e| match e {
                // Only invalid-data failures are rewrapped; the rest keep their identity.
                PressPodsError::InvalidData { message, .. } => {
                    PressPodsError::failed("probe PressPods intro", message)
                }
                other => other,
            })?;
        let chunk_gap: TempFile = self.chain.make_silence_wav(CHUNK_GAP_SEC).await?;
        let section_gap: TempFile = self.chain.make_silence_wav(SECTION_GAP_SEC).await?;

        let mut segments: Vec<Segment> = Vec::new();
        let mut pieces_out: Vec<PreparedChunk> = Vec::new();
        let mut chapters = Vec::new();
        let mut chunk_stats = Vec::new();
        let mut speech_offset = 0.0_f64;
        let mut total_chunks: usize = sections
            .iter()
            .map(|s| chunk_text(&s.body, profile.target, profile.max).len())
            .sum();

        for (s, section) in sections.iter().enumerate() {
            if s > 0 {
                segments.push(Segment::SectionGap);
                speech_offset += SECTION_GAP_SEC;
            }
            if sections.len() > 1 {
                chapters.push(Chapter::new(
                    intro_duration + speech_offset,
                    section
                        .title
                        .clone()
                        .unwrap_or_else(|| "Introduction".to_owned()),
                ));
            }
            let mut first_in_section = true;
            for chunk in chunk_text(&section.body, profile.target, profile.max) {
                let pieces = self
                    .synthesize_chunk_adaptive(provider, &chunk, stt, ckpt.as_ref(), 0)
                    .await?;
                total_chunks += pieces.len().saturating_sub(1);
                for piece in pieces {
                    if !first_in_section {
                        segments.push(Segment::ChunkGap);
                        speech_offset += CHUNK_GAP_SEC;
                    }
                    first_in_section = false;
                    let start_time_seconds = intro_duration + speech_offset;
                    let duration_seconds = piece.chunk.duration_seconds;
                    speech_offset += duration_seconds;
                    let index = chunk_stats.len();
                    let chars = utf16_len(&piece.text);
                    #[allow(clippy::cast_precision_loss)]
                    let sec_per_char = if chars > 0 {
                        duration_seconds / chars as f64
                    } else {
                        0.0
                    };
                    #[allow(clippy::cast_precision_loss)]
                    chunk_stats.push(ChunkStat {
                        index: i64::try_from(index).unwrap_or(i64::MAX),
                        section_index: i64::try_from(s).unwrap_or(i64::MAX),
                        section_title: section.title.clone(),
                        char_count: i64::try_from(chars).unwrap_or(i64::MAX),
                        text: piece.text,
                        duration_seconds,
                        start_time_seconds,
                        sec_per_char,
                        attempts: i64::from(piece.attempts),
                        coverage: piece.coverage.map(|c| c.coverage),
                        word_ratio: piece.coverage.map(|c| c.word_ratio),
                        expected_words: piece.coverage.map(|c| c.expected_words as f64),
                        resplit: piece.resplit.then_some(true),
                        resplit_depth: piece.resplit_depth.map(i64::from),
                        extra: Default::default(),
                    });
                    segments.push(Segment::Piece(pieces_out.len()));
                    pieces_out.push(piece.chunk);
                    tracing::info!(target: LOG, "Synthesized chunk {}/{total_chunks}", index + 1);
                }
            }
        }

        let paths: Vec<&Path> = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::ChunkGap => Some(chunk_gap.path()),
                Segment::SectionGap => Some(section_gap.path()),
                Segment::Piece(i) => pieces_out.get(*i).map(|p| p.wav.path()),
            })
            .collect();
        let audio = self.chain.assemble_episode(&paths, &intro).await?;
        tracing::info!(target: LOG, audio_bytes = audio.len(), chapters = chapters.len(), "Speech synthesized");
        #[allow(clippy::cast_precision_loss)]
        let synthesized_seconds = (self.clock.now_ms() - start) as f64 / 1000.0;
        Ok(SynthesisResult {
            audio,
            voice_name: provider.voice_name().to_owned(),
            voice_provider: provider.provider_name().to_owned(),
            synthesized_seconds,
            chapters,
            chunks: chunk_stats,
        })
    }
}
