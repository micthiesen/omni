//! The sherpa-onnx backend (safe wrapper only): Silero VAD, 3D-Speaker CAM++
//! embeddings, pyannote segmentation 3.0 diarization and the Parakeet TDT 0.6B
//! v3 int8 transducer, all on CPU, configured so embeddings match the enrolled
//! voiceprints.

use std::path::Path;
use std::sync::Mutex;

use sherpa_onnx::{
    FastClusteringConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineSpeakerDiarization,
    OfflineSpeakerDiarizationConfig, OfflineSpeakerSegmentationModelConfig,
    OfflineSpeakerSegmentationPyannoteModelConfig, OfflineTransducerModelConfig,
    SileroVadModelConfig, SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig,
    VadModelConfig, VoiceActivityDetector,
};

use crate::audio::SAMPLE_RATE;
use crate::speech::{
    DiarizedSegment, ModelFiles, SpeechBackend, SpeechRecognitionError, SpeechSpan,
};

const VAD_WINDOW: usize = 512;
const VAD_BUFFER_SECONDS: f32 = 120.0;
/// Diarization merges speaker clusters closer than this; sherpa's 0.5 default
/// split one streamer into a dozen clusters on 8-minute stream windows, while
/// 0.8-1.0 gave the same few speakers.
const DIARIZATION_CLUSTER_THRESHOLD: f32 = 0.9;
/// Segmentation window step as a fraction of its 10 s window. The 0.1 default
/// cost four times as much on stream audio and clustered worse.
const SEGMENTATION_WINDOW_SHIFT: f32 = 0.5;

fn path_string(path: &Path) -> Option<String> {
    Some(path.to_string_lossy().into_owned())
}

#[allow(clippy::cast_possible_wrap)]
const SAMPLE_RATE_I32: i32 = SAMPLE_RATE as i32;

/// VAD and speaker embeddings: everything voice matching and enrollment need.
/// The extractor is created once; a VAD is created per call because it is
/// stateful.
pub struct SherpaSpeaker {
    vad_config: VadModelConfig,
    speaker: Mutex<SpeakerEmbeddingExtractor>,
}

fn init_error(what: &str) -> SpeechRecognitionError {
    SpeechRecognitionError::new(
        "initialize livestream speech models",
        format!("sherpa-onnx could not create the {what}"),
    )
}

impl SherpaSpeaker {
    pub fn new(vad: &Path, speaker: &Path) -> Result<Self, SpeechRecognitionError> {
        let extractor = SpeakerEmbeddingExtractor::create(&SpeakerEmbeddingExtractorConfig {
            model: path_string(speaker),
            num_threads: 1,
            debug: false,
            provider: Some("cpu".to_owned()),
        })
        .ok_or_else(|| init_error("speaker embedding extractor"))?;
        let vad_config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: path_string(vad),
                threshold: 0.5,
                min_silence_duration: 0.35,
                min_speech_duration: 0.25,
                #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                window_size: VAD_WINDOW as i32,
                max_speech_duration: 20.0,
            },
            sample_rate: SAMPLE_RATE_I32,
            num_threads: 1,
            provider: Some("cpu".to_owned()),
            debug: false,
            ..VadModelConfig::default()
        };
        Ok(Self {
            vad_config,
            speaker: Mutex::new(extractor),
        })
    }

    /// VAD speech segments of at least one second.
    pub fn speech_segments(
        &self,
        samples: &[f32],
    ) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        let vad = VoiceActivityDetector::create(&self.vad_config, VAD_BUFFER_SECONDS).ok_or_else(
            || SpeechRecognitionError::new("extract livestream speech", "VAD creation failed"),
        )?;
        for chunk in samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(chunk);
        }
        vad.flush();
        let mut segments = Vec::new();
        while let Some(segment) = vad.front() {
            if segment.samples().len() >= SAMPLE_RATE as usize {
                segments.push(segment.samples().to_vec());
            }
            vad.pop();
        }
        Ok(segments)
    }

    /// VAD speech spans of any length, as sample ranges of `samples`.
    pub fn speech_spans(&self, samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        let vad = VoiceActivityDetector::create(&self.vad_config, VAD_BUFFER_SECONDS).ok_or_else(
            || SpeechRecognitionError::new("find speech spans", "VAD creation failed"),
        )?;
        let mut spans = Vec::new();
        // Drained while feeding, so the detector's buffer never has to hold
        // more than one utterance of a long chunk.
        let mut drain = |vad: &VoiceActivityDetector| {
            while let Some(segment) = vad.front() {
                let start = usize::try_from(segment.start()).unwrap_or(0);
                spans.push(SpeechSpan {
                    start,
                    end: start + segment.samples().len(),
                });
                vad.pop();
            }
        };
        for chunk in samples.chunks(VAD_WINDOW) {
            vad.accept_waveform(chunk);
            drain(&vad);
        }
        vad.flush();
        drain(&vad);
        Ok(spans)
    }

    /// One speaker embedding.
    pub fn embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        let extractor = lock(&self.speaker);
        let stream = extractor.create_stream().ok_or_else(|| {
            SpeechRecognitionError::new("prepare speaker embedding", "stream creation failed")
        })?;
        // No input_finished, so embeddings stay comparable with the enrolled
        // voiceprints.
        stream.accept_waveform(SAMPLE_RATE_I32, samples);
        if !extractor.is_ready(&stream) {
            return Err(SpeechRecognitionError::new(
                "compute speaker embedding",
                "Speaker sample is too short for an embedding",
            ));
        }
        extractor.compute(&stream).ok_or_else(|| {
            SpeechRecognitionError::new("compute speaker embedding", "no embedding returned")
        })
    }
}

/// Native models: [`SherpaSpeaker`] plus the transducer recognizer and the
/// diarizer.
pub struct SherpaBackend {
    speaker: SherpaSpeaker,
    recognizer: Mutex<OfflineRecognizer>,
    diarizer: Mutex<OfflineSpeakerDiarization>,
}

fn create_diarizer(
    files: &ModelFiles,
) -> Result<OfflineSpeakerDiarization, SpeechRecognitionError> {
    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: path_string(&files.segmentation),
                window_shift_ratio: SEGMENTATION_WINDOW_SHIFT,
            },
            num_threads: 2,
            debug: false,
            provider: Some("cpu".to_owned()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: path_string(&files.speaker),
            num_threads: 2,
            debug: false,
            provider: Some("cpu".to_owned()),
        },
        clustering: FastClusteringConfig {
            threshold: DIARIZATION_CLUSTER_THRESHOLD,
            ..FastClusteringConfig::default()
        },
        ..OfflineSpeakerDiarizationConfig::default()
    };
    let diarizer =
        OfflineSpeakerDiarization::create(&config).ok_or_else(|| init_error("speaker diarizer"))?;
    if diarizer.sample_rate() != SAMPLE_RATE_I32 {
        return Err(SpeechRecognitionError::new(
            "initialize livestream speech models",
            format!(
                "the diarizer expects {} Hz audio, not {SAMPLE_RATE} Hz",
                diarizer.sample_rate()
            ),
        ));
    }
    Ok(diarizer)
}

impl SherpaBackend {
    pub fn new(files: &ModelFiles) -> Result<Self, SpeechRecognitionError> {
        let speaker = SherpaSpeaker::new(&files.vad, &files.speaker)?;
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer = OfflineTransducerModelConfig {
            encoder: path_string(&files.encoder),
            decoder: path_string(&files.decoder),
            joiner: path_string(&files.joiner),
        };
        config.model_config.tokens = path_string(&files.tokens);
        config.model_config.num_threads = 3;
        config.model_config.provider = Some("cpu".to_owned());
        config.model_config.debug = false;
        config.model_config.model_type = Some("nemo_transducer".to_owned());
        config.decoding_method = Some("greedy_search".to_owned());
        let recognizer =
            OfflineRecognizer::create(&config).ok_or_else(|| init_error("offline recognizer"))?;
        let diarizer = create_diarizer(files)?;
        Ok(Self {
            speaker,
            recognizer: Mutex::new(recognizer),
            diarizer: Mutex::new(diarizer),
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SpeechBackend for SherpaBackend {
    fn speech_segments(&self, samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        self.speaker.speech_segments(samples)
    }

    fn embedding(&self, samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        self.speaker.embedding(samples)
    }

    fn speech_spans(&self, samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        self.speaker.speech_spans(samples)
    }

    fn transcribe(&self, samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        let recognizer = lock(&self.recognizer);
        let stream = recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE_I32, samples);
        recognizer.decode(&stream);
        stream.get_result().map(|r| r.text).ok_or_else(|| {
            SpeechRecognitionError::new("transcribe livestream audio", "no recognition result")
        })
    }

    fn diarize(&self, samples: &[f32]) -> Result<Vec<DiarizedSegment>, SpeechRecognitionError> {
        let diarizer = lock(&self.diarizer);
        let result = diarizer.process(samples).ok_or_else(|| {
            SpeechRecognitionError::new("diarize livestream audio", "no diarization result")
        })?;
        Ok(result
            .sort_by_start_time()
            .into_iter()
            .filter_map(|segment| {
                Some(DiarizedSegment {
                    start: f64::from(segment.start),
                    end: f64::from(segment.end),
                    speaker: u32::try_from(segment.speaker).ok()?,
                })
            })
            .collect())
    }
}
