//! Reading a transcript: timestamped text pages and search. Pure functions
//! over the stored segments.

use crate::records::{TranscriptSegmentData, TranscriptState, YoutubeTranscriptData};

/// A line of a page gathers segments until it holds this many characters...
const LINE_CHARS: usize = 400;
/// ...or spans this much of the video.
const LINE_SPAN_MS: i64 = 60_000;
const EXCERPT_CHARS: usize = 500;

/// `m:ss` under an hour, `h:mm:ss` from there.
pub fn timestamp(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    let (hours, minutes, seconds) = (total / 3600, total % 3600 / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// What part of a transcript to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    /// Index of the first segment to consider.
    pub cursor: usize,
    /// Only speech that ends after this.
    pub start_ms: Option<i64>,
    /// Only speech that starts before this.
    pub end_ms: Option<i64>,
    pub max_chars: usize,
}

/// One page of transcript text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// `[timestamp] text` lines.
    pub text: String,
    /// Start of the first and end of the last segment on the page.
    pub range_ms: Option<(i64, i64)>,
    /// Index of the first segment after the page that is still in range.
    pub next_cursor: Option<usize>,
}

/// A stretch without text this long is pointed out in the transcript.
pub const QUIET_FLAG_MS: i64 = 30_000;
/// Audio this much shorter than the video's reported length is pointed out.
const EARLY_END_MS: i64 = 5_000;

/// Why a stretch of the video has no text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    /// The voice detector found speech and the recognizer returned nothing:
    /// words are missing here.
    UnrecognizedSpeech,
    /// Neither found speech for [`QUIET_FLAG_MS`] or more: silence or music,
    /// or speech both missed.
    NoSpeechRecognized,
    /// The audio stopped before the video's reported length.
    AudioEndsEarly,
}

/// A stretch of the video the transcript has no text for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flag {
    pub start_ms: i64,
    pub end_ms: i64,
    pub kind: FlagKind,
}

impl Flag {
    /// The line that stands for the stretch in the transcript text.
    pub fn note(&self) -> String {
        let seconds = (self.end_ms - self.start_ms).max(0) / 1000;
        match self.kind {
            FlagKind::UnrecognizedSpeech => {
                format!("(speech detected but not transcribed: {seconds} s)")
            }
            FlagKind::NoSpeechRecognized => format!(
                "(no speech recognized for {})",
                timestamp(self.end_ms - self.start_ms)
            ),
            FlagKind::AudioEndsEarly => {
                format!("(the audio ends here, {seconds} s before the video's reported length)")
            }
        }
    }
}

/// Every stretch of the transcribed audio without text, in time order: the
/// recorded gaps, quiet stretches of [`QUIET_FLAG_MS`] or more between text,
/// and audio that ends before the video does. Timestamps alone would show
/// none of these.
pub fn flags(row: &YoutubeTranscriptData) -> Vec<Flag> {
    let mut flags: Vec<Flag> = row
        .gaps
        .iter()
        .map(|gap| Flag {
            start_ms: gap.start_ms,
            end_ms: gap.end_ms,
            kind: FlagKind::UnrecognizedSpeech,
        })
        .collect();
    let mut covered: Vec<(i64, i64)> = row
        .segments
        .iter()
        .map(|s| (s.start_ms, s.end_ms))
        .chain(row.gaps.iter().map(|g| (g.start_ms, g.end_ms)))
        .collect();
    covered.sort_unstable();
    covered.push((row.transcribed_ms, row.transcribed_ms));
    let mut quiet_from = 0;
    for (start, end) in covered {
        if start - quiet_from >= QUIET_FLAG_MS {
            flags.push(Flag {
                start_ms: quiet_from,
                end_ms: start,
                kind: FlagKind::NoSpeechRecognized,
            });
        }
        quiet_from = quiet_from.max(end);
    }
    if row.state == TranscriptState::Ready
        && let Some(duration) = row.duration_ms
        && duration - row.transcribed_ms > EARLY_END_MS
    {
        flags.push(Flag {
            start_ms: row.transcribed_ms,
            end_ms: duration,
            kind: FlagKind::AudioEndsEarly,
        });
    }
    flags.sort_by_key(|flag| (flag.start_ms, flag.end_ms));
    flags
}

const UNDETECTED_NOTE: &str = "(not detected as speech) ";

/// Lines of `[timestamp] text`, each gathering consecutive segments, cut at
/// `max_chars` on a segment boundary. At least one segment is returned when
/// any is in range, so a page always advances. Each flag in range is a line
/// of its own where it falls, and text the voice detector did not take for
/// speech is marked, so the text itself shows where it is uncertain or
/// missing.
pub fn page(segments: &[TranscriptSegmentData], flags: &[Flag], request: PageRequest) -> Page {
    let in_range = |start_ms: i64, end_ms: i64| {
        request.start_ms.is_none_or(|start| end_ms > start)
            && request.end_ms.is_none_or(|end| start_ms < end)
    };
    // The segment each flag comes before.
    let positions: Vec<usize> = flags
        .iter()
        .map(|flag| segments.partition_point(|s| s.start_ms < flag.start_ms))
        .collect();
    let mut page = Page::default();
    let mut line_start_ms = 0;
    let mut line_chars = 0;
    let mut page_chars = 0;
    // The next segment starts a line of its own.
    let mut break_line = true;
    let cover = |page: &mut Page, start_ms: i64, end_ms: i64| {
        page.range_ms = Some(match page.range_ms {
            Some((start, end)) => (start, end.max(end_ms)),
            None => (start_ms, end_ms),
        });
    };
    for index in request.cursor..=segments.len() {
        let notes: Vec<&Flag> = flags
            .iter()
            .zip(&positions)
            .filter(|(flag, at)| **at == index && in_range(flag.start_ms, flag.end_ms))
            .map(|(flag, _)| flag)
            .collect();
        let segment = segments.get(index);
        let shown = segment.filter(|s| in_range(s.start_ms, s.end_ms));
        let line = shown.map(|segment| {
            let new_line = break_line
                || !notes.is_empty()
                || segment.undetected
                || line_chars >= LINE_CHARS
                || segment.start_ms - line_start_ms >= LINE_SPAN_MS;
            let head = match (new_line, segment.undetected) {
                (false, _) => String::new(),
                (true, false) => format!("[{}] ", timestamp(segment.start_ms)),
                (true, true) => format!("[{}] {UNDETECTED_NOTE}", timestamp(segment.start_ms)),
            };
            (new_line, head)
        });
        if let (Some(segment), Some((_, head))) = (shown, &line) {
            let added = 1 + head.chars().count() + segment.text.chars().count();
            if !page.text.is_empty() && page_chars + added > request.max_chars {
                page.next_cursor = Some(index);
                break;
            }
        }
        for flag in notes {
            if !page.text.is_empty() {
                page.text.push('\n');
            }
            let note = format!("[{}] {}", timestamp(flag.start_ms), flag.note());
            page_chars += 1 + note.chars().count();
            page.text.push_str(&note);
            cover(&mut page, flag.start_ms, flag.end_ms);
        }
        let Some(segment) = segment else {
            break;
        };
        let Some((new_line, head)) = line else {
            // Segments are in time order: nothing after the range is in it.
            if request.end_ms.is_some_and(|end| segment.start_ms >= end) {
                break;
            }
            continue;
        };
        if !page.text.is_empty() {
            page.text.push(if new_line { '\n' } else { ' ' });
        }
        if new_line {
            line_start_ms = segment.start_ms;
            line_chars = 0;
        }
        line_chars += segment.text.chars().count() + 1;
        page_chars += 1 + head.chars().count() + segment.text.chars().count();
        page.text.push_str(&head);
        page.text.push_str(&segment.text);
        cover(&mut page, segment.start_ms, segment.end_ms);
        // Marked text stands alone, so the mark covers exactly that text.
        break_line = segment.undetected;
    }
    page
}

/// One search match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index of the matching segment.
    pub index: usize,
    /// Start of the excerpt.
    pub start_ms: i64,
    /// The matching segment with the one before and after it, bounded.
    pub excerpt: String,
    /// The query's words appear together, in order.
    pub exact_phrase: bool,
}

/// Lowercase letters and digits, everything else a single space.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
    }
    out.trim_end().to_owned()
}

fn excerpt(segments: &[TranscriptSegmentData], index: usize) -> (i64, String) {
    let first = index.saturating_sub(1);
    let last = (index + 1).min(segments.len() - 1);
    let joined = segments[first..=last]
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut clipped: String = joined.chars().take(EXCERPT_CHARS).collect();
    if joined.chars().count() > EXCERPT_CHARS {
        clipped.push_str("...");
    }
    (segments[first].start_ms, clipped)
}

/// Segments that hold every word of `query` (ignoring case and punctuation;
/// a word also matches inside a longer one), alone or together with the next
/// segment so a sentence split across two still matches. In time order, with
/// the total number of matches.
pub fn search(segments: &[TranscriptSegmentData], query: &str, limit: usize) -> (Vec<Hit>, usize) {
    let phrase = normalize(query);
    let terms: Vec<&str> = phrase.split(' ').filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return (Vec::new(), 0);
    }
    let normalized: Vec<String> = segments.iter().map(|s| normalize(&s.text)).collect();
    let all_in = |text: &str| terms.iter().all(|term| text.contains(term));
    let mut hits = Vec::new();
    let mut total = 0;
    for (index, own) in normalized.iter().enumerate() {
        let next = normalized.get(index + 1).map_or("", String::as_str);
        let matched = if all_in(own) {
            true
        } else {
            // Spread over this segment and the next; a match wholly inside
            // the next segment is reported there.
            let pair = format!("{own} {next}");
            !next.is_empty()
                && !all_in(next)
                && all_in(&pair)
                && terms.iter().any(|term| own.contains(term))
        };
        if !matched {
            continue;
        }
        total += 1;
        if hits.len() < limit {
            let (start_ms, text) = excerpt(segments, index);
            hits.push(Hit {
                index,
                start_ms,
                excerpt: text,
                exact_phrase: format!("{own} {next}").contains(&phrase),
            });
        }
    }
    (hits, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(start_s: i64, end_s: i64, text: &str) -> TranscriptSegmentData {
        TranscriptSegmentData {
            start_ms: start_s * 1000,
            end_ms: end_s * 1000,
            text: text.to_owned(),
            undetected: false,
        }
    }

    fn talk() -> Vec<TranscriptSegmentData> {
        vec![
            segment(0, 8, "Welcome back to the channel."),
            segment(9, 20, "Today we look at interest rates and"),
            segment(20, 31, "why the central bank held them steady."),
            segment(70, 80, "Inflation, however, is still above target."),
            segment(3700, 3710, "Thanks for watching."),
        ]
    }

    fn request(cursor: usize, max_chars: usize) -> PageRequest {
        PageRequest {
            cursor,
            start_ms: None,
            end_ms: None,
            max_chars,
        }
    }

    #[test]
    fn timestamps_gain_hours_only_when_needed() {
        assert_eq!(timestamp(0), "0:00");
        assert_eq!(timestamp(754_999), "12:34");
        assert_eq!(timestamp(3_723_000), "1:02:03");
    }

    #[test]
    fn a_page_groups_segments_into_timestamped_lines() {
        let page = page(&talk(), &[], request(0, 10_000));
        assert_eq!(
            page.text,
            "[0:00] Welcome back to the channel. Today we look at interest rates and why the central bank held them steady.\n\
             [1:10] Inflation, however, is still above target.\n\
             [1:01:40] Thanks for watching."
        );
        assert_eq!(page.range_ms, Some((0, 3_710_000)));
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn a_page_stops_at_the_character_budget_and_resumes_at_its_cursor() {
        let segments = talk();
        let first = page(&segments, &[], request(0, 80));
        assert_eq!(
            first.text,
            "[0:00] Welcome back to the channel. Today we look at interest rates and"
        );
        assert_eq!(first.next_cursor, Some(2));
        let rest = page(&segments, &[], request(2, 10_000));
        assert!(rest.text.starts_with("[0:20] why the central bank"));
        assert_eq!(rest.next_cursor, None);

        // A budget smaller than one segment still returns that segment.
        let tiny = page(&segments, &[], request(0, 5));
        assert_eq!(tiny.text, "[0:00] Welcome back to the channel.");
        assert_eq!(tiny.next_cursor, Some(1));
    }

    #[test]
    fn a_time_range_selects_overlapping_speech() {
        let slice = page(
            &talk(),
            &[],
            PageRequest {
                cursor: 0,
                start_ms: Some(25_000),
                end_ms: Some(75_000),
                max_chars: 10_000,
            },
        );
        assert_eq!(
            slice.text,
            "[0:20] why the central bank held them steady. Inflation, however, is still above target."
        );
        assert_eq!(slice.range_ms, Some((20_000, 80_000)));
        assert_eq!(slice.next_cursor, None);

        let empty = page(
            &talk(),
            &[],
            PageRequest {
                cursor: 0,
                start_ms: Some(100_000),
                end_ms: Some(200_000),
                max_chars: 100,
            },
        );
        assert_eq!(empty, Page::default());
    }

    #[test]
    fn search_ignores_case_and_punctuation() {
        let (hits, total) = search(&talk(), "INFLATION however", 10);
        assert_eq!(total, 1);
        assert_eq!(hits[0].index, 3);
        assert!(hits[0].exact_phrase);
        // The excerpt carries the neighbouring segments and starts at the first.
        assert_eq!(hits[0].start_ms, 20_000);
        assert!(
            hits[0]
                .excerpt
                .contains("held them steady. Inflation, however")
        );
    }

    #[test]
    fn search_matches_a_sentence_split_across_two_segments_once() {
        let (hits, total) = search(&talk(), "interest rates and why the central bank", 10);
        assert_eq!(total, 1);
        assert_eq!(hits[0].index, 1);
        assert!(hits[0].exact_phrase);

        // Words in any order still match, but not as a phrase.
        let (loose, _) = search(&talk(), "bank central", 10);
        assert_eq!(loose.len(), 1);
        assert_eq!(loose[0].index, 2);
        assert!(!loose[0].exact_phrase);
    }

    #[test]
    fn search_reports_the_total_beyond_its_limit() {
        let (hits, total) = search(&talk(), "the", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(total, 2);
        assert_eq!(search(&talk(), "?!", 5), (vec![], 0));
        assert_eq!(search(&talk(), "tariffs", 5), (vec![], 0));
        assert_eq!(search(&[], "anything", 5), (vec![], 0));
    }

    fn row(
        segments: Vec<TranscriptSegmentData>,
        gaps: &[(i64, i64)],
        audio_s: i64,
        duration_s: i64,
        state: TranscriptState,
    ) -> YoutubeTranscriptData {
        YoutubeTranscriptData {
            video_id: "video".into(),
            state,
            title: None,
            channel: None,
            duration_ms: Some(duration_s * 1000),
            upload_date: None,
            requested_at: 0,
            started_at: None,
            finished_at: None,
            last_read_at: 0,
            transcribed_ms: audio_s * 1000,
            attempts: 1,
            segments,
            gaps: gaps
                .iter()
                .map(|(start, end)| crate::records::TranscriptGapData {
                    start_ms: start * 1000,
                    end_ms: end * 1000,
                })
                .collect(),
            speech_ms: Some(0),
            model: None,
            error_code: None,
            error: None,
            extra: Default::default(),
        }
    }

    fn kinds(flags: &[Flag]) -> Vec<(i64, i64, FlagKind)> {
        flags
            .iter()
            .map(|f| (f.start_ms / 1000, f.end_ms / 1000, f.kind))
            .collect()
    }

    #[test]
    fn stretches_without_text_are_flagged_not_implied_by_timestamps() {
        // Text at 40-50 s and 60-70 s of a 200 s video whose audio ran 150 s,
        // with detected speech at 100-106 s that produced no text.
        let segments = vec![segment(40, 50, "First."), segment(60, 70, "Second.")];
        let flagged = flags(&row(
            segments.clone(),
            &[(100, 106)],
            150,
            200,
            TranscriptState::Ready,
        ));
        assert_eq!(
            kinds(&flagged),
            vec![
                (0, 40, FlagKind::NoSpeechRecognized),
                (70, 100, FlagKind::NoSpeechRecognized),
                (100, 106, FlagKind::UnrecognizedSpeech),
                (106, 150, FlagKind::NoSpeechRecognized),
                (150, 200, FlagKind::AudioEndsEarly),
            ]
        );

        // A running job is flagged only up to what it has transcribed, and
        // shorter quiet stretches or a few seconds of trailing difference are not flags.
        let running = flags(&row(
            segments.clone(),
            &[],
            75,
            200,
            TranscriptState::Transcribing,
        ));
        assert_eq!(kinds(&running), vec![(0, 40, FlagKind::NoSpeechRecognized)]);
        let tight = vec![segment(5, 50, "First."), segment(60, 196, "Second.")];
        assert_eq!(
            flags(&row(tight, &[], 197, 200, TranscriptState::Ready)),
            vec![]
        );
    }

    #[test]
    fn flags_and_undetected_text_are_written_into_the_page() {
        let mut faint = segment(120, 130, "Is anyone there?");
        faint.undetected = true;
        let segments = vec![
            segment(40, 50, "First."),
            segment(52, 58, "Second."),
            faint,
            segment(131, 140, "Third."),
        ];
        let data = row(
            segments.clone(),
            &[(70, 76)],
            150,
            200,
            TranscriptState::Ready,
        );
        let flagged = flags(&data);
        let whole = page(&segments, &flagged, request(0, 10_000));
        assert_eq!(
            whole.text,
            "[0:00] (no speech recognized for 0:40)\n\
             [0:40] First. Second.\n\
             [1:10] (speech detected but not transcribed: 6 s)\n\
             [1:16] (no speech recognized for 0:44)\n\
             [2:00] (not detected as speech) Is anyone there?\n\
             [2:11] Third.\n\
             [2:30] (the audio ends here, 50 s before the video's reported length)"
        );
        assert_eq!(whole.range_ms, Some((0, 200_000)));
        assert_eq!(whole.next_cursor, None);

        // Paging repeats no flag and loses none.
        let first = page(&segments, &flagged, request(0, 70));
        assert_eq!(
            first.text,
            "[0:00] (no speech recognized for 0:40)\n[0:40] First. Second."
        );
        assert_eq!(first.next_cursor, Some(2));
        let rest = page(&segments, &flagged, request(2, 10_000));
        assert_eq!(format!("{}\n{}", first.text, rest.text), whole.text);

        // A slice that holds no text still shows why.
        let quiet = page(
            &segments,
            &flagged,
            PageRequest {
                cursor: 0,
                start_ms: Some(80_000),
                end_ms: Some(110_000),
                max_chars: 1000,
            },
        );
        assert_eq!(quiet.text, "[1:16] (no speech recognized for 0:44)");
    }
}
