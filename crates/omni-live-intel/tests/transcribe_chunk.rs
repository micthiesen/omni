//! Chunked transcription of a long recording: speech spans merge into
//! sentence-sized pieces, and an utterance cut by the end of a chunk is left
//! for the next chunk.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use omni_live_intel::speech::{
    DEFAULT_SPEAKER_THRESHOLD, DiarizedSegment, LocalSpeechRuntime, LocalTranscriber,
    SpeechBackend, SpeechRecognitionError, SpeechSpan, TRANSCRIPTION_MODEL, merge_spans,
};
use omni_runtime::ports::SpeechTranscriber as _;

const RATE: usize = 16_000;

fn span(start_seconds: f64, end_seconds: f64) -> SpeechSpan {
    SpeechSpan {
        start: (start_seconds * RATE as f64) as usize,
        end: (end_seconds * RATE as f64) as usize,
    }
}

/// The spans are given; a span's text is its length in tenths of a second,
/// and a span whose first sample is negative is silent.
struct Backend(Vec<SpeechSpan>);

impl SpeechBackend for Backend {
    fn speech_segments(&self, _samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        Ok(vec![])
    }

    fn embedding(&self, _samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        Ok(vec![])
    }

    fn speech_spans(&self, _samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        Ok(self.0.clone())
    }

    fn transcribe(&self, samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        if samples.first().is_some_and(|s| *s < 0.0) {
            return Ok("   ".to_owned());
        }
        Ok(format!(" {} ", samples.len() * 10 / RATE))
    }

    fn diarize(&self, _samples: &[f32]) -> Result<Vec<DiarizedSegment>, SpeechRecognitionError> {
        Ok(vec![])
    }
}

fn runtime(spans: Vec<SpeechSpan>) -> LocalSpeechRuntime<Backend> {
    LocalSpeechRuntime::with_backend(Backend(spans), None, DEFAULT_SPEAKER_THRESHOLD)
}

#[test]
fn close_spans_merge_until_thirty_seconds() {
    let merged = merge_spans(
        &[
            span(12.0, 14.0),
            span(1.0, 3.0),
            span(3.5, 6.0),
            // One second after the previous span: a new sentence.
            span(7.0, 9.0),
        ],
        60 * RATE,
    );
    assert_eq!(
        merged,
        vec![span(1.0, 6.0), span(7.0, 9.0), span(12.0, 14.0)]
    );

    // Back-to-back 20 s spans (the detector's maximum) do not grow past 30 s.
    let long = merge_spans(&[span(0.0, 20.0), span(20.0, 40.0)], 60 * RATE);
    assert_eq!(long, vec![span(0.0, 20.0), span(20.0, 40.0)]);

    // Spans are clipped to the audio, and empty ones dropped.
    let clipped = merge_spans(&[span(8.0, 12.0), span(11.0, 11.0)], 10 * RATE);
    assert_eq!(clipped, vec![span(8.0, 10.0)]);
}

#[test]
fn an_utterance_cut_by_the_chunk_end_waits_for_the_next_chunk() {
    let samples = vec![0.0_f32; 100 * RATE];
    let runtime = runtime(vec![span(2.0, 5.0), span(90.0, 99.8)]);
    let chunk = runtime.transcribe_chunk(&samples, false).expect("chunk");
    assert_eq!(chunk.consumed_samples, 90 * RATE);
    assert_eq!(chunk.segments.len(), 1);
    assert_eq!(chunk.segments[0].start_sample, 2 * RATE);
    assert_eq!(chunk.segments[0].end_sample, 5 * RATE);
    assert_eq!(chunk.segments[0].text, "30");

    // The last chunk of the recording transcribes everything.
    let last = runtime.transcribe_chunk(&samples, true).expect("chunk");
    assert_eq!(last.consumed_samples, samples.len());
    assert_eq!(last.segments.len(), 2);
    assert_eq!(last.segments[1].text, "98");
}

#[test]
fn a_tail_that_would_stall_the_recording_is_transcribed() {
    let samples = vec![0.0_f32; 100 * RATE];
    // Only the final span is held back, however long the speech before it.
    let chunk = runtime(vec![span(40.0, 60.0), span(60.0, 80.0), span(80.0, 100.0)])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert_eq!(chunk.consumed_samples, 80 * RATE);
    assert_eq!(chunk.segments.len(), 2);

    // A final span starting in the first half: holding it back would advance
    // the recording by less than half a chunk.
    let stalled = runtime(vec![span(30.0, 100.0)])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert_eq!(stalled.consumed_samples, samples.len());
    assert_eq!(stalled.segments.len(), 1);
}

#[test]
fn silence_and_wordless_spans_produce_no_segments() {
    let mut samples = vec![0.0_f32; 20 * RATE];
    samples[3 * RATE] = -1.0;
    let chunk = runtime(vec![span(3.0, 6.0)])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert!(chunk.segments.is_empty());
    assert_eq!(chunk.consumed_samples, samples.len());

    let quiet = runtime(vec![])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert_eq!(quiet.consumed_samples, samples.len());
}

#[tokio::test]
async fn the_port_runs_the_runtime_off_the_async_threads() {
    let port = LocalTranscriber(Arc::new(runtime(vec![span(1.0, 2.0)])));
    assert_eq!(port.model(), TRANSCRIPTION_MODEL);
    let chunk = port
        .transcribe_chunk(Arc::new(vec![0.0; 10 * RATE]), true)
        .await
        .expect("chunk");
    assert_eq!(chunk.segments.len(), 1);
    assert_eq!(chunk.segments[0].text, "10");
}
