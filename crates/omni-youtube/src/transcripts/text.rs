//! Reading a transcript: timestamped text pages and search. Pure functions
//! over the stored segments.

use crate::records::TranscriptSegmentData;

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

/// Lines of `[timestamp] text`, each gathering consecutive segments, cut at
/// `max_chars` on a segment boundary. At least one segment is returned when
/// any is in range, so a page always advances.
pub fn page(segments: &[TranscriptSegmentData], request: PageRequest) -> Page {
    let in_range = |segment: &TranscriptSegmentData| {
        request.start_ms.is_none_or(|start| segment.end_ms > start)
            && request.end_ms.is_none_or(|end| segment.start_ms < end)
    };
    let mut page = Page::default();
    let mut line_start_ms = 0;
    let mut line_chars = 0;
    let mut page_chars = 0;
    for (index, segment) in segments.iter().enumerate().skip(request.cursor) {
        if !in_range(segment) {
            // Segments are in time order: nothing after the range is in it.
            if request.end_ms.is_some_and(|end| segment.start_ms >= end) {
                break;
            }
            continue;
        }
        let new_line = page.text.is_empty()
            || line_chars >= LINE_CHARS
            || segment.start_ms - line_start_ms >= LINE_SPAN_MS;
        let prefix = if new_line {
            let stamp = timestamp(segment.start_ms);
            if page.text.is_empty() {
                format!("[{stamp}] ")
            } else {
                format!("\n[{stamp}] ")
            }
        } else {
            " ".to_owned()
        };
        let added = prefix.chars().count() + segment.text.chars().count();
        if !page.text.is_empty() && page_chars + added > request.max_chars {
            page.next_cursor = Some(index);
            break;
        }
        if new_line {
            line_start_ms = segment.start_ms;
            line_chars = 0;
        }
        line_chars += segment.text.chars().count() + 1;
        page_chars += added;
        page.text.push_str(&prefix);
        page.text.push_str(&segment.text);
        page.range_ms = Some((
            page.range_ms.map_or(segment.start_ms, |(start, _)| start),
            segment.end_ms,
        ));
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
        let page = page(&talk(), request(0, 10_000));
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
        let first = page(&segments, request(0, 80));
        assert_eq!(
            first.text,
            "[0:00] Welcome back to the channel. Today we look at interest rates and"
        );
        assert_eq!(first.next_cursor, Some(2));
        let rest = page(&segments, request(2, 10_000));
        assert!(rest.text.starts_with("[0:20] why the central bank"));
        assert_eq!(rest.next_cursor, None);

        // A budget smaller than one segment still returns that segment.
        let tiny = page(&segments, request(0, 5));
        assert_eq!(tiny.text, "[0:00] Welcome back to the channel.");
        assert_eq!(tiny.next_cursor, Some(1));
    }

    #[test]
    fn a_time_range_selects_overlapping_speech() {
        let slice = page(
            &talk(),
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
}
