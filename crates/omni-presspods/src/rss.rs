//! The PressPods podcast feed.
//!
//! The output is a public contract read by podcast clients and frozen by
//! `tests/golden/rss.xml` (the `podcast` npm package's serialization), so this
//! module writes exact bytes: element order, CDATA use, self-closing empty elements, entity
//! escaping and the attribute order of the `<rss>` root. A general RSS
//! library would produce an equivalent but not identical document.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;

use crate::model::PressPodsEpisode;

/// Episodes listed in the feed, newest first.
pub const FEED_EPISODE_LIMIT: usize = 50;

const FEED_TITLE: &str = "PressPods";
const FEED_DESCRIPTION: &str = "A podcast of the latest news from the web, read aloud by a robot";
const FEED_AUTHOR: &str = "Michael Thiesen";
const FEED_SITE_URL: &str = "https://github.com/micthiesen/omni";
const FEED_LANGUAGE: &str = "en";
const FEED_GENERATOR: &str = "Podcast for Node";

static HEADING_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^\s*##\s+(.+?)\s*$").ok());
static QUOTE_START_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(&gt;)+\s*").ok());

/// Escapes `&`, `<`, `>`, `"` and `'` for XML text and attributes.
pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

/// The `xml` package's `escapeForXML` (same set, single pass).
fn escape_for_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// An already-escaped line starting with `&gt;`.
pub fn is_block_quote(line: &str) -> bool {
    QUOTE_START_RE.as_ref().is_some_and(|re| re.is_match(line))
}

/// Escapes narration for HTML show notes, rendering
/// `## Heading` chapter markers bold and blockquote lines italic.
pub fn prepare_text_for_rss(text: Option<&str>) -> String {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return String::new();
    };
    escape_xml(text)
        .split('\n')
        .map(|line| {
            if let Some(heading) = HEADING_RE
                .as_ref()
                .and_then(|re| re.captures(line))
                .and_then(|c| c.get(1))
            {
                return format!("<b>{}</b>", heading.as_str());
            }
            if is_block_quote(line) {
                let rest = QUOTE_START_RE
                    .as_ref()
                    .map(|re| re.replace(line, "").into_owned())
                    .unwrap_or_else(|| line.to_owned());
                return format!("<i>{rest}</i>");
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

/// Truncates to `length` UTF-16 units with a `"..."` suffix.
pub fn truncate(s: &str, length: usize) -> String {
    const SUFFIX: &str = "...";
    if omni_core::js::utf16_len(s) > length {
        let head = omni_core::js::utf16_slice(s, 0, length.saturating_sub(SUFFIX.len()));
        format!("{head}{SUFFIX}")
    } else {
        s.to_owned()
    }
}

/// `podcast`'s `toDurationString`: `hh:mm:ss`, each part the last two digits.
fn duration_string(seconds: f64) -> String {
    fn pad(n: f64) -> String {
        let padded = format!("0{}", omni_core::js::number_to_string(n));
        let start = padded.len().saturating_sub(2);
        padded[start..].to_owned()
    }
    let hh = (seconds / 3600.0).floor();
    let remain = seconds % 3600.0;
    let mm = (remain / 60.0).floor();
    let ss = (remain % 60.0).floor();
    format!("{}:{}:{}", pad(hh), pad(mm), pad(ss))
}

/// JS `Date#toUTCString` / `toGMTString`.
pub fn utc_string(ms: i64) -> String {
    match jiff::Timestamp::from_millisecond(ms) {
        Ok(ts) => ts
            .to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%a, %d %b %Y %H:%M:%S GMT")
            .to_string(),
        Err(_) => "Invalid Date".to_owned(),
    }
}

/// One node in the `xml` package's object model.
enum Content {
    /// No content: rendered self-closing (`<x/>`).
    Empty,
    /// Escaped text, possibly empty (`<x></x>`).
    Text(String),
    /// `_cdata`, with `]]>` split.
    Cdata(String),
    Children(Vec<Element>),
}

struct Element {
    name: &'static str,
    attrs: Vec<(&'static str, String)>,
    content: Content,
}

impl Element {
    fn text(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            content: Content::Text(value.into()),
        }
    }

    /// `{ name: { _cdata: value } }`; a falsy (empty) value renders `<name/>`.
    fn cdata(name: &'static str, value: &str) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            content: if value.is_empty() {
                Content::Empty
            } else {
                Content::Cdata(value.to_owned())
            },
        }
    }

    fn empty(name: &'static str, attrs: Vec<(&'static str, String)>) -> Self {
        Self {
            name,
            attrs,
            content: Content::Empty,
        }
    }

    fn children(name: &'static str, children: Vec<Element>) -> Self {
        Self {
            name,
            attrs: Vec::new(),
            content: Content::Children(children),
        }
    }

    fn render(&self, out: &mut String) {
        out.push('<');
        out.push_str(self.name);
        for (key, value) in &self.attrs {
            out.push(' ');
            let _ = write!(out, "{key}=\"{}\"", escape_for_xml(value));
        }
        match &self.content {
            Content::Empty => out.push_str("/>"),
            Content::Text(text) => {
                out.push('>');
                out.push_str(&escape_for_xml(text));
                let _ = write!(out, "</{}>", self.name);
            }
            Content::Cdata(text) => {
                out.push('>');
                let block = format!("<![CDATA[{text}").replace("]]>", "]]]]><![CDATA[>");
                out.push_str(&block);
                out.push_str("]]>");
                let _ = write!(out, "</{}>", self.name);
            }
            Content::Children(children) => {
                out.push('>');
                for child in children {
                    child.render(out);
                }
                let _ = write!(out, "</{}>", self.name);
            }
        }
    }
}

fn item(base_url: &str, episode: &PressPodsEpisode) -> Element {
    let excerpt = match &episode.excerpt {
        Some(excerpt) => excerpt.clone(),
        None => truncate(&episode.content, 255),
    };
    let safe_url = escape_xml(&episode.article_url);
    let description = format!(
        "{}<br><a href=\"{safe_url}\">{safe_url}</a><br><br>{}",
        escape_xml(&excerpt),
        prepare_text_for_rss(Some(&episode.content))
    );
    let author = episode.author.as_deref().filter(|a| !a.is_empty());

    let title = if episode.title.is_empty() {
        "No title"
    } else {
        episode.title.as_str()
    };
    let mut children = vec![
        Element::cdata("title", title),
        Element::cdata("description", &description),
        Element {
            name: "guid",
            attrs: vec![("isPermaLink", "false".to_owned())],
            content: Content::Text(episode.episode_id.clone()),
        },
        Element::cdata("dc:creator", author.unwrap_or(FEED_AUTHOR)),
        Element::text("pubDate", utc_string(episode.created_at)),
        Element::empty(
            "enclosure",
            vec![
                (
                    "url",
                    format!("{base_url}/pods/audio/{}", episode.audio_file),
                ),
                ("length", episode.file_bytes.to_string()),
                ("type", "audio/mpeg".to_owned()),
            ],
        ),
    ];
    if let Some(author) = author {
        children.push(Element::text("itunes:author", author));
    }
    if !excerpt.is_empty() {
        children.push(Element::text("itunes:subtitle", excerpt));
    }
    children.push(Element::text("itunes:summary", description));
    children.push(Element::text("itunes:explicit", "false"));
    if let Some(duration) = episode
        .duration_seconds
        .filter(|d| *d != 0.0 && !d.is_nan())
    {
        children.push(Element::text("itunes:duration", duration_string(duration)));
    }
    if let Some(image) = episode.lead_image_url.as_deref().filter(|i| !i.is_empty()) {
        children.push(Element::empty(
            "itunes:image",
            vec![("href", image.to_owned())],
        ));
    }
    Element::children("item", children)
}

/// Builds the feed. `base_url` is the public origin enclosures are fetched
/// from (no trailing slash); `episodes` are newest first; `now_ms` is the
/// `lastBuildDate`.
pub fn build_feed(base_url: &str, episodes: &[PressPodsEpisode], now_ms: i64) -> String {
    let image_url = format!("{base_url}/pods/logo.jpeg");
    let mut channel = vec![
        Element::cdata("title", FEED_TITLE),
        Element::cdata("description", FEED_DESCRIPTION),
        Element::text("link", FEED_SITE_URL),
        Element::text("generator", FEED_GENERATOR),
        Element::text("lastBuildDate", utc_string(now_ms)),
        Element::cdata("author", FEED_AUTHOR),
        Element::cdata("language", FEED_LANGUAGE),
        Element::text("itunes:author", FEED_AUTHOR),
        Element::text("itunes:summary", FEED_DESCRIPTION),
        Element::children(
            "itunes:owner",
            vec![
                Element::text("itunes:name", FEED_AUTHOR),
                Element::text("itunes:email", ""),
            ],
        ),
        Element::text("itunes:explicit", "false"),
        Element::empty("itunes:image", vec![("href", image_url)]),
    ];
    channel.extend(
        episodes
            .iter()
            .take(FEED_EPISODE_LIMIT)
            .map(|episode| item(base_url, episode)),
    );
    let rss = Element {
        name: "rss",
        attrs: vec![
            ("xmlns:dc", "http://purl.org/dc/elements/1.1/".to_owned()),
            (
                "xmlns:content",
                "http://purl.org/rss/1.0/modules/content/".to_owned(),
            ),
            ("xmlns:atom", "http://www.w3.org/2005/Atom".to_owned()),
            ("version", "2.0".to_owned()),
            (
                "xmlns:itunes",
                "http://www.itunes.com/dtds/podcast-1.0.dtd".to_owned(),
            ),
            ("xmlns:psc", "http://podlove.org/simple-chapters".to_owned()),
            (
                "xmlns:podcast",
                "https://podcastindex.org/namespace/1.0".to_owned(),
            ),
        ],
        content: Content::Children(vec![Element::children("channel", channel)]),
    };
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    rss.render(&mut out);
    out
}

/// The feed ETag: the newest episode id (`"no-episodes"` when empty), quoted.
pub fn feed_etag(episodes: &[PressPodsEpisode]) -> String {
    let latest = episodes
        .first()
        .map(|e| e.episode_id.as_str())
        .unwrap_or("no-episodes");
    format!("\"{latest}\"")
}

#[cfg(test)]
mod rss_text_spec {
    //! Feed text helper cases.
    use super::*;

    #[test]
    fn escapes_xml_entities() {
        assert_eq!(
            prepare_text_for_rss(Some("a & b <c>")),
            "a &amp; b &lt;c&gt;"
        );
    }

    #[test]
    fn italicizes_blockquote_lines_and_strips_the_quote_prefix() {
        assert_eq!(
            prepare_text_for_rss(Some("intro\n> quoted text")),
            "intro<br><i>quoted text</i>"
        );
    }

    #[test]
    fn renders_chapter_markers_as_bold_headings_not_literal_markdown() {
        assert_eq!(
            prepare_text_for_rss(Some("## Background\nbody text")),
            "<b>Background</b><br>body text"
        );
    }

    #[test]
    fn does_not_treat_mid_line_as_a_heading() {
        assert_eq!(
            prepare_text_for_rss(Some("rated it 4## stars")),
            "rated it 4## stars"
        );
    }

    #[test]
    fn returns_empty_string_for_undefined() {
        assert_eq!(prepare_text_for_rss(None), "");
    }

    #[test]
    fn detects_escaped_quote_prefixes() {
        assert!(is_block_quote("&gt; quoted"));
        assert!(!is_block_quote("plain"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_strings_match_the_podcast_package() {
        assert_eq!(duration_string(523.4351), "00:08:43");
        assert_eq!(duration_string(3_725.0), "01:02:05");
        assert_eq!(duration_string(123.0 * 3600.0), "23:00:00");
    }

    #[test]
    fn truncation_counts_utf16_units() {
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn utc_strings_match_js() {
        assert_eq!(
            utc_string(1_767_225_600_000),
            "Thu, 01 Jan 2026 00:00:00 GMT"
        );
    }
}
