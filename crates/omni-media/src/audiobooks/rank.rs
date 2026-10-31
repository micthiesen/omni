//! Ranks audiobook releases for Prologue.
//!
//! MyAnonamouse titles read `Title by Author [LANG / FORMAT]` (formats can be
//! several, `MP3 M4B`). In order, a release ranks higher when it: matches
//! every word of the query; has seeders; is in the preferred language; is
//! unabridged; is m4b (one chaptered file, which Prologue plays best), then
//! m4a/aac, then mp3, then other audio; has at least three seeders;
//! is freeleech; and has more seeders. Ebook-only releases are dropped.

use std::cmp::Reverse;
use std::sync::LazyLock;

use regex::Regex;

/// The fields ranking reads from one release.
#[derive(Clone, Debug, Default)]
pub struct Candidate {
    pub title: String,
    pub seeders: Option<i64>,
    pub freeleech: bool,
}

/// What ranking learned about a release.
#[derive(Clone, Debug, PartialEq)]
pub struct Assessment {
    pub book: Option<String>,
    pub author: Option<String>,
    pub language: Option<String>,
    /// Lowercase, best first.
    pub formats: Vec<String>,
    pub notes: Vec<String>,
    /// Matches the query, is seeded, in the preferred language and unabridged.
    pub good_match: bool,
    key: (u8, bool, u8, bool, u8, bool, bool, i64),
}

static MAM_TITLE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^(?P<book>.+?) by (?P<author>.+?)\s*\[(?P<lang>[A-Za-z]{2,3})\s*/\s*(?P<fmt>[^\]]+)\]\s*$").ok()
});
static FORMAT_WORD: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(m4b|m4a|aac|mp3|flac|ogg|opus|wma|epub|mobi|azw3|pdf|cbz|cbr)\b").ok()
});

const EBOOK_FORMATS: [&str; 6] = ["epub", "mobi", "azw3", "pdf", "cbz", "cbr"];
const STOP_WORDS: [&str; 9] = [
    "the",
    "a",
    "an",
    "of",
    "and",
    "by",
    "audiobook",
    "unabridged",
    "book",
];
const HEALTHY_SEEDERS: i64 = 3;

fn format_rank(format: &str) -> u8 {
    match format {
        "m4b" => 4,
        "m4a" | "aac" => 3,
        "mp3" => 2,
        "flac" | "ogg" | "opus" | "wma" => 1,
        _ => 0,
    }
}

/// Lowercase words of letters and digits.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Query words that carry meaning.
fn query_words(query: &str) -> Vec<String> {
    let all = words(query);
    let meaningful: Vec<String> = all
        .iter()
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .cloned()
        .collect();
    if meaningful.is_empty() {
        all
    } else {
        meaningful
    }
}

fn language_name(code: &str) -> String {
    match code {
        "ENG" => "English",
        "DUT" | "NLD" => "Dutch",
        "DAN" => "Danish",
        "GER" | "DEU" => "German",
        "FRE" | "FRA" => "French",
        "SPA" => "Spanish",
        "ITA" => "Italian",
        "SWE" => "Swedish",
        "NOR" => "Norwegian",
        "POL" => "Polish",
        "POR" => "Portuguese",
        "RUS" => "Russian",
        "JPN" => "Japanese",
        "CHI" | "ZHO" => "Chinese",
        _ => code,
    }
    .to_owned()
}

/// `None` for a release that is not an audiobook (ebook formats only).
pub fn assess(query: &str, language: &str, candidate: &Candidate) -> Option<Assessment> {
    let parsed = MAM_TITLE
        .as_ref()
        .and_then(|re| re.captures(&candidate.title));
    let book = parsed
        .as_ref()
        .and_then(|c| c.name("book"))
        .map(|m| m.as_str().trim().to_owned());
    let author = parsed
        .as_ref()
        .and_then(|c| c.name("author"))
        .map(|m| m.as_str().trim().to_owned());
    let lang = parsed
        .as_ref()
        .and_then(|c| c.name("lang"))
        .map(|m| m.as_str().to_uppercase());
    let format_text = parsed
        .as_ref()
        .and_then(|c| c.name("fmt"))
        .map_or(candidate.title.as_str(), |m| m.as_str());
    let mut formats: Vec<String> = FORMAT_WORD
        .as_ref()
        .map(|re| {
            re.find_iter(format_text)
                .map(|m| m.as_str().to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    formats.sort_by_key(|f| Reverse(format_rank(f)));
    formats.dedup();
    if !formats.is_empty() && formats.iter().all(|f| EBOOK_FORMATS.contains(&f.as_str())) {
        return None;
    }
    formats.retain(|f| !EBOOK_FORMATS.contains(&f.as_str()));

    let wanted = query_words(query);
    let have = words(&candidate.title);
    let matched = wanted.iter().filter(|w| have.contains(w)).count();
    let relevance = if matched == wanted.len() {
        2
    } else if matched * 2 >= wanted.len() {
        1
    } else {
        0
    };
    let seeders = candidate.seeders.unwrap_or(0);
    let preferred = language.to_uppercase();
    let language_score = match lang.as_deref() {
        Some(l) if l == preferred => 2,
        None => 1,
        Some(_) => 0,
    };
    let abridged = have.iter().any(|w| w == "abridged");
    let best_format = formats.first().map_or(0, |f| format_rank(f));

    let mut notes = Vec::new();
    match &lang {
        Some(l) if *l == preferred => notes.push(language_name(l)),
        Some(l) => notes.push(format!(
            "{}, not {}",
            language_name(l),
            language_name(&preferred)
        )),
        None => notes.push("language not stated".to_owned()),
    }
    match formats.first().map(String::as_str) {
        Some("m4b") => notes.push("m4b: chaptered, best in Prologue".to_owned()),
        Some(f) => notes.push(format!("{f}: works, but not one chaptered file")),
        None => notes.push("format not stated".to_owned()),
    }
    if abridged {
        notes.push("abridged".to_owned());
    }
    match seeders {
        0 => notes.push("no seeders: will not download".to_owned()),
        n if n < HEALTHY_SEEDERS => notes.push(format!("only {n} seeder(s): may be slow")),
        _ => {}
    }
    if candidate.freeleech {
        notes.push("freeleech: no ratio cost".to_owned());
    }
    if relevance < 2 {
        notes.push("title does not match every query word".to_owned());
    }

    Some(Assessment {
        good_match: relevance == 2 && seeders > 0 && language_score == 2 && !abridged,
        book,
        author,
        language: lang,
        formats,
        notes,
        key: (
            relevance,
            seeders > 0,
            language_score,
            !abridged,
            best_format,
            seeders >= HEALTHY_SEEDERS,
            candidate.freeleech,
            seeders,
        ),
    })
}

/// Sorts best first; ties keep their order.
pub fn sort<T>(items: &mut [(Assessment, T)]) {
    items.sort_by_key(|(a, _)| Reverse(a.key));
}

#[cfg(test)]
mod tests {
    use super::{Candidate, assess, sort};

    fn candidate(title: &str, seeders: i64) -> Candidate {
        Candidate {
            title: title.to_owned(),
            seeders: Some(seeders),
            freeleech: false,
        }
    }

    fn ranked(query: &str, candidates: &[Candidate]) -> Vec<String> {
        let mut items: Vec<_> = candidates
            .iter()
            .filter_map(|c| assess(query, "ENG", c).map(|a| (a, c.title.clone())))
            .collect();
        sort(&mut items);
        items.into_iter().map(|(_, t)| t).collect()
    }

    /// The live MyAnonamouse results for "Project Hail Mary" on 2026-10-31.
    #[test]
    fn prefers_english_m4b_over_more_seeded_mp3_and_other_languages() {
        let results = [
            candidate("Project Hail Mary by Andy Weir [DUT / M4B]", 41),
            candidate("Project Hail Mary by Andy Weir [ENG / M4B]", 95),
            candidate("Project Hail Mary by Andy Weir [DUT / MP3]", 17),
            candidate("Project Hail Mary by Andy Weir [DAN / MP3]", 86),
            candidate("Project Hail Mary by Andy Weir [ENG / MP3]", 1364),
        ];
        assert_eq!(
            ranked("Project Hail Mary", &results),
            [
                "Project Hail Mary by Andy Weir [ENG / M4B]",
                "Project Hail Mary by Andy Weir [ENG / MP3]",
                "Project Hail Mary by Andy Weir [DUT / M4B]",
                "Project Hail Mary by Andy Weir [DAN / MP3]",
                "Project Hail Mary by Andy Weir [DUT / MP3]",
            ]
        );
    }

    #[test]
    fn dead_abridged_and_mismatched_releases_sink() {
        let results = [
            candidate("Piranesi by Susanna Clarke [ENG / M4B]", 0),
            candidate("Piranesi (Abridged) by Susanna Clarke [ENG / M4B]", 40),
            candidate("Jonathan Strange by Susanna Clarke [ENG / M4B]", 300),
            candidate("Piranesi by Susanna Clarke [ENG / MP3]", 2),
        ];
        assert_eq!(
            ranked("Piranesi Susanna Clarke", &results),
            [
                "Piranesi by Susanna Clarke [ENG / MP3]",
                "Piranesi (Abridged) by Susanna Clarke [ENG / M4B]",
                "Piranesi by Susanna Clarke [ENG / M4B]",
                "Jonathan Strange by Susanna Clarke [ENG / M4B]",
            ]
        );
    }

    #[test]
    fn parses_the_mam_title_and_drops_ebooks() {
        let a = assess(
            "wager",
            "ENG",
            &candidate("The Wager by David Grann [ENG / MP3 M4B]", 10),
        )
        .expect("audiobook");
        assert_eq!(a.book.as_deref(), Some("The Wager"));
        assert_eq!(a.author.as_deref(), Some("David Grann"));
        assert_eq!(a.formats, ["m4b", "mp3"]);
        assert!(a.good_match);
        assert!(
            assess(
                "wager",
                "ENG",
                &candidate("The Wager by David Grann [ENG / EPUB]", 9)
            )
            .is_none()
        );
    }
}
