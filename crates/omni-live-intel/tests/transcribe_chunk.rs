//! Chunked transcription of a long recording: the recognizer hears every
//! sample, the voice detector only chooses where to cut, and detected speech
//! without text is reported instead of dropped.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use omni_live_intel::speech::{
    DEFAULT_SPEAKER_THRESHOLD, DiarizedSegment, LocalSpeechRuntime, LocalTranscriber,
    SpeechBackend, SpeechRecognitionError, SpeechSpan, TRANSCRIPTION_MODEL, plan_windows,
};
use omni_runtime::ports::{SpeechGap, SpeechTranscriber as _};

const RATE: usize = 16_000;

fn at(seconds: f64) -> usize {
    (seconds * RATE as f64) as usize
}

fn span(start_seconds: f64, end_seconds: f64) -> SpeechSpan {
    SpeechSpan {
        start: at(start_seconds),
        end: at(end_seconds),
    }
}

/// The detector's spans are given. The recognizer's text is the length of
/// what it was handed, in tenths of a second; audio starting with a negative
/// sample has no words. Every call is recorded.
struct Backend {
    spans: Vec<SpeechSpan>,
    heard: Arc<Mutex<Vec<usize>>>,
}

impl SpeechBackend for Backend {
    fn speech_segments(&self, _samples: &[f32]) -> Result<Vec<Vec<f32>>, SpeechRecognitionError> {
        Ok(vec![])
    }

    fn embedding(&self, _samples: &[f32]) -> Result<Vec<f32>, SpeechRecognitionError> {
        Ok(vec![])
    }

    fn speech_spans(&self, _samples: &[f32]) -> Result<Vec<SpeechSpan>, SpeechRecognitionError> {
        Ok(self.spans.clone())
    }

    fn transcribe(&self, samples: &[f32]) -> Result<String, SpeechRecognitionError> {
        self.heard.lock().unwrap().push(samples.len());
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
    recording_runtime(spans).0
}

/// The runtime and the lengths of the audio its recognizer was handed.
fn recording_runtime(
    spans: Vec<SpeechSpan>,
) -> (LocalSpeechRuntime<Backend>, Arc<Mutex<Vec<usize>>>) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let backend = Backend {
        spans,
        heard: Arc::clone(&heard),
    };
    (
        LocalSpeechRuntime::with_backend(backend, None, DEFAULT_SPEAKER_THRESHOLD),
        heard,
    )
}

/// Window bounds in seconds.
fn bounds(spans: &[SpeechSpan], seconds: f64, last: bool) -> (Vec<(f64, f64)>, f64) {
    let plan = plan_windows(spans, at(seconds), last);
    let windows = plan
        .windows
        .iter()
        .map(|w| (w.start as f64 / RATE as f64, w.end as f64 / RATE as f64))
        .collect();
    (windows, plan.consumed as f64 / RATE as f64)
}

#[test]
fn the_windows_cover_every_sample_whatever_the_detector_found() {
    let cases: Vec<(Vec<SpeechSpan>, f64, bool)> = vec![
        (vec![], 120.0, false),
        (vec![], 0.4, true),
        (vec![span(0.0, 120.0)], 120.0, false),
        (vec![span(11.0, 14.0), span(40.0, 41.0)], 19.13, true),
        (
            (0..60)
                .map(|n| span(n as f64 * 2.0, n as f64 * 2.0 + 1.5))
                .collect(),
            120.0,
            false,
        ),
        // Overlapping, unsorted and out-of-range spans.
        (
            vec![span(50.0, 500.0), span(3.0, 9.0), span(5.0, 7.0)],
            100.0,
            true,
        ),
    ];
    for (spans, seconds, last) in cases {
        let total = at(seconds);
        let plan = plan_windows(&spans, total, last);
        let mut next = 0;
        for window in &plan.windows {
            assert_eq!(window.start, next, "{spans:?}");
            assert!(window.end > window.start);
            assert!(window.end - window.start <= 31 * RATE);
            assert!(window.speech_samples <= window.end - window.start);
            if let Some(speech) = window.speech {
                assert!(speech.start >= window.start && speech.end <= window.end);
            }
            next = window.end;
        }
        assert_eq!(next, plan.consumed, "{spans:?}");
        assert!(plan.consumed >= total / 2);
        if last {
            assert_eq!(plan.consumed, total);
        }
    }
}

#[test]
fn a_window_is_cut_in_the_widest_pause_between_sentences() {
    // Pauses at 5.9-6.1, 10-10.4, 15-17 and 19-19.5 s.
    let spans = [
        span(0.5, 5.9),
        span(6.1, 10.0),
        span(10.4, 15.0),
        span(17.0, 19.0),
        span(19.5, 40.0),
    ];
    let (windows, consumed) = bounds(&spans, 40.0, true);
    // The two-second pause wins over the later, shorter one; the pause before
    // eight seconds is too early to cut at.
    assert_eq!(windows, vec![(0.0, 16.0), (16.0, 40.0)]);
    assert_eq!(consumed, 40.0);
}

#[test]
fn without_a_pause_in_range_the_last_pause_or_the_limit_cuts() {
    // One pause at 25 s, outside the preferred range but before the limit.
    let (late, _) = bounds(&[span(0.0, 24.8), span(25.2, 70.0)], 70.0, true);
    assert_eq!(late, vec![(0.0, 25.0), (25.0, 55.0), (55.0, 70.0)]);

    // Unbroken audio is cut at the limit, and a sliver is not left over.
    let (unbroken, _) = bounds(&[], 60.5, true);
    assert_eq!(unbroken, vec![(0.0, 30.0), (30.0, 60.5)]);
}

#[test]
fn speech_the_detector_missed_is_still_transcribed() {
    // The detector hears eleven seconds; the recording runs nineteen.
    let samples = vec![0.0_f32; at(19.0)];
    let heard_some = runtime(vec![span(0.2, 11.0)]);
    let chunk = heard_some.transcribe_chunk(&samples, true).expect("chunk");
    assert_eq!(chunk.consumed_samples, samples.len());
    assert_eq!(chunk.segments.len(), 1);
    // The recognizer was handed all nineteen seconds.
    assert_eq!(chunk.segments[0].text, "190");
    assert!(chunk.segments[0].detected);
    assert_eq!(chunk.speech_samples, at(11.0) - at(0.2));

    // With no detected speech at all, the text is kept and marked.
    let nothing = runtime(vec![]);
    let chunk = nothing.transcribe_chunk(&samples, true).expect("chunk");
    assert_eq!(chunk.segments.len(), 1);
    assert_eq!(chunk.segments[0].text, "190");
    assert!(!chunk.segments[0].detected);
    assert_eq!(
        (chunk.segments[0].start_sample, chunk.segments[0].end_sample),
        (0, at(19.0))
    );
    assert_eq!(chunk.speech_samples, 0);
    assert!(chunk.gaps.is_empty());
}

#[test]
fn every_sample_reaches_the_recognizer_exactly_once() {
    let samples = vec![0.0_f32; at(120.0)];
    let spans: Vec<SpeechSpan> = (0..20)
        .map(|n| span(n as f64 * 6.0 + 1.0, n as f64 * 6.0 + 5.0))
        .collect();
    let (runtime, heard) = recording_runtime(spans);
    let chunk = runtime.transcribe_chunk(&samples, true).expect("chunk");
    let heard: usize = heard.lock().unwrap().iter().sum();
    assert_eq!(heard, samples.len());
    assert_eq!(chunk.consumed_samples, samples.len());
    // Segment times are the detected speech inside each window, in order.
    for pair in chunk.segments.windows(2) {
        assert!(pair[0].end_sample <= pair[1].start_sample);
    }
    assert_eq!(chunk.segments[0].start_sample, at(1.0));
}

#[test]
fn speech_cut_by_the_chunk_end_waits_for_the_next_chunk() {
    let samples = vec![0.0_f32; at(120.0)];
    let spans = vec![span(2.0, 60.0), span(62.0, 100.0), span(104.0, 119.8)];
    let chunk = runtime(spans.clone())
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    // The next chunk starts in the pause before the unfinished speech.
    assert_eq!(chunk.consumed_samples, at(102.0));
    assert!(chunk.segments.iter().all(|s| s.end_sample <= at(102.0)));

    // The last chunk of the recording transcribes everything.
    let last = runtime(spans)
        .transcribe_chunk(&samples, true)
        .expect("chunk");
    assert_eq!(last.consumed_samples, samples.len());

    // Speech running from the first half to the end cannot be held back:
    // the chunk would advance by less than half its length.
    let stalled = runtime(vec![span(30.0, 120.0)])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert_eq!(stalled.consumed_samples, samples.len());

    // A chunk ending in silence is consumed whole.
    let quiet = runtime(vec![span(5.0, 80.0)])
        .transcribe_chunk(&samples, false)
        .expect("chunk");
    assert_eq!(quiet.consumed_samples, samples.len());
}

#[test]
fn detected_speech_without_text_is_reported_as_a_gap() {
    let mut samples = vec![0.0_f32; at(20.0)];
    samples[0] = -1.0;
    let chunk = runtime(vec![span(3.0, 9.0)])
        .transcribe_chunk(&samples, true)
        .expect("chunk");
    assert!(chunk.segments.is_empty());
    assert_eq!(
        chunk.gaps,
        vec![SpeechGap {
            start_sample: at(3.0),
            end_sample: at(9.0),
        }]
    );
    assert_eq!(chunk.speech_samples, at(6.0));

    // Half a second of detected sound without words is not a gap, and
    // silence the recognizer also finds empty is nothing at all.
    let blip = runtime(vec![span(3.0, 3.5)])
        .transcribe_chunk(&samples, true)
        .expect("chunk");
    assert!(blip.gaps.is_empty() && blip.segments.is_empty());
    let silence = runtime(vec![])
        .transcribe_chunk(&samples, true)
        .expect("chunk");
    assert!(silence.gaps.is_empty() && silence.segments.is_empty());
    assert_eq!(silence.consumed_samples, samples.len());
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
    assert_eq!(chunk.segments[0].text, "100");
    assert_eq!(chunk.speech_samples, RATE);
}
