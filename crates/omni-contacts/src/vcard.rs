//! Lossless vCard text.
//!
//! A [`Card`] keeps every logical line's exact source text (folding and line
//! terminator included). Serializing an unedited card reproduces the input
//! byte for byte, and an edit replaces, removes or inserts only the lines it
//! targets: a changed value keeps the line's original group, name and
//! parameter text. Photos, Apple `X-` extensions and properties this module
//! does not understand therefore survive, as vCard text, any edit that does
//! not name them. Data a client keeps outside the vCard is out of reach.

use std::collections::{BTreeMap, HashMap};

use omni_core::digest::sha256_hex;

/// Lines over this many octets are folded when rendered (RFC 6350 3.2).
const FOLD_OCTETS: usize = 75;

/// The vCard text could not be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum VcardError {
    #[error("the resource is not a vCard (no BEGIN:VCARD ... END:VCARD)")]
    NotVcard,
    #[error("the resource holds more than one vCard")]
    MultipleCards,
    #[error("the resource has content outside BEGIN:VCARD ... END:VCARD")]
    OutsideContent,
    #[error("an edited line contains a line break")]
    LineBreak,
}

/// One parameter: `TYPE=HOME,pref`, or a bare `HOME` (vCard 2.1 style).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: Option<String>,
    /// Values with surrounding quotes removed.
    pub values: Vec<String>,
    /// The parameter as written (`None` for one this module created); a
    /// re-rendered line writes it back unchanged.
    pub raw: Option<String>,
}

impl Param {
    pub fn new(name: &str, value: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            values: vec![value.to_owned()],
            raw: None,
        }
    }

    pub fn is(&self, name: &str) -> bool {
        self.name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    }
}

/// A parsed content line: `[group.]NAME;params:value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prop {
    pub group: Option<String>,
    /// As written.
    pub name: String,
    pub params: Vec<Param>,
    /// The raw (still escaped) value.
    pub value: String,
    /// Byte offset of the `:` separating the prefix from the value.
    colon: usize,
}

impl Prop {
    pub fn name_upper(&self) -> String {
        self.name.to_ascii_uppercase()
    }

    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    pub fn in_group(&self, group: &str) -> bool {
        self.group
            .as_deref()
            .is_some_and(|g| g.eq_ignore_ascii_case(group))
    }

    /// The value as text (escapes decoded).
    pub fn text(&self) -> String {
        unescape(&self.value)
    }

    /// The `;`-separated components, each decoded.
    pub fn components(&self) -> Vec<String> {
        split_unescaped(&self.value, ';')
            .iter()
            .map(|c| unescape(c))
            .collect()
    }

    /// Every TYPE value (named `TYPE` params and bare params), uppercased.
    pub fn types(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|p| p.name.is_none() || p.is("TYPE"))
            .flat_map(|p| p.values.iter())
            .flat_map(|v| v.split(','))
            .map(|v| v.trim().to_ascii_uppercase())
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// TYPE values as written (case kept), for re-rendering a line.
    pub fn raw_types(&self) -> Vec<String> {
        self.params
            .iter()
            .filter(|p| p.name.is_none() || p.is("TYPE"))
            .flat_map(|p| p.values.iter())
            .flat_map(|v| v.split(','))
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// A parameter's first value.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|p| p.is(name))
            .and_then(|p| p.values.first())
            .map(String::as_str)
    }
}

/// One logical line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The exact source bytes (folds and terminator included); `None` for a
    /// line this module rendered, which is folded on output.
    raw: Option<String>,
    /// The unfolded text without its terminator.
    text: String,
    prop: Option<Prop>,
}

impl Line {
    fn rendered(text: String) -> Self {
        let prop = parse_prop(&text);
        Self {
            raw: None,
            text,
            prop,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn prop(&self) -> Option<&Prop> {
        self.prop.as_ref()
    }
}

/// A vCard as logical lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    lines: Vec<Line>,
    /// The terminator used for rendered lines (the source's, else CRLF).
    eol: &'static str,
}

impl Card {
    /// Parses one vCard. Anything that is not a content line is kept verbatim.
    pub fn parse(text: &str) -> Result<Self, VcardError> {
        let eol = if text.contains("\r\n") || !text.contains('\n') {
            "\r\n"
        } else {
            "\n"
        };
        let mut lines: Vec<Line> = Vec::new();
        for physical in text.split_inclusive('\n') {
            let content = physical.trim_end_matches('\n').trim_end_matches('\r');
            let continues = content.starts_with([' ', '\t']);
            match lines.last_mut() {
                Some(last) if continues => {
                    if let Some(raw) = last.raw.as_mut() {
                        raw.push_str(physical);
                    }
                    last.text.push_str(&content[1..]);
                }
                _ => lines.push(Line {
                    raw: Some(physical.to_owned()),
                    text: content.to_owned(),
                    prop: None,
                }),
            }
        }
        for (index, line) in lines.iter_mut().enumerate() {
            let text = if index == 0 {
                line.text.trim_start_matches('\u{feff}')
            } else {
                &line.text
            };
            line.prop = parse_prop(text);
        }
        let card = Self { lines, eol };
        let begins = card.indices("BEGIN", Some("VCARD"));
        let ends = card.indices("END", Some("VCARD"));
        let (begin, end) = match (begins.as_slice(), ends.as_slice()) {
            ([begin], [end]) if begin < end => (*begin, *end),
            ([], _) | (_, []) => return Err(VcardError::NotVcard),
            _ if begins.len() > 1 => return Err(VcardError::MultipleCards),
            _ => return Err(VcardError::NotVcard),
        };
        // Nothing but blank lines may surround the card: trailing content is
        // not part of the contact and must not be read as if it were.
        let blank = |l: &Line| l.text.trim_matches(['\u{feff}', ' ', '\t']).is_empty();
        if !card.lines[..begin].iter().all(blank) || !card.lines[end + 1..].iter().all(blank) {
            return Err(VcardError::OutsideContent);
        }
        Ok(card)
    }

    fn indices(&self, name: &str, value: Option<&str>) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.prop.as_ref().is_some_and(|p| {
                    p.is(name) && value.is_none_or(|v| p.value.trim().eq_ignore_ascii_case(v))
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// The exact text: source lines verbatim, rendered lines folded.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            match &line.raw {
                Some(raw) => out.push_str(raw),
                None => out.push_str(&fold(&line.text, self.eol)),
            }
        }
        out
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    pub fn eol(&self) -> &'static str {
        self.eol
    }

    /// Parsed properties with their line index.
    pub fn props(&self) -> impl Iterator<Item = (usize, &Prop)> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.prop.as_ref().map(|p| (i, p)))
    }

    /// Properties named `name` (case-insensitive), with their index.
    pub fn named<'a, 'n>(
        &'a self,
        name: &'n str,
    ) -> impl Iterator<Item = (usize, &'a Prop)> + use<'a, 'n> {
        self.props().filter(move |(_, p)| p.is(name))
    }

    pub fn first(&self, name: &str) -> Option<&Prop> {
        self.named(name).next().map(|(_, p)| p)
    }

    pub fn prop(&self, index: usize) -> Option<&Prop> {
        self.lines.get(index).and_then(|l| l.prop.as_ref())
    }

    pub fn version(&self) -> Option<String> {
        self.first("VERSION").map(|p| p.value.trim().to_owned())
    }

    pub fn uid(&self) -> Option<String> {
        self.first("UID")
            .map(|p| p.text().trim().to_owned())
            .filter(|u| !u.is_empty())
    }

    /// The index of the END:VCARD line.
    pub fn end_index(&self) -> usize {
        self.indices("END", Some("VCARD"))
            .last()
            .copied()
            .unwrap_or(self.lines.len())
    }

    /// Lines sharing `group` (case-insensitive), with their index.
    pub fn group_members<'a, 'g>(
        &'a self,
        group: &'g str,
    ) -> impl Iterator<Item = (usize, &'a Prop)> + use<'a, 'g> {
        self.props().filter(move |(_, p)| p.in_group(group))
    }

    /// The number after the highest `itemN` group, or `None` when no larger
    /// number exists.
    pub fn next_item_number(&self) -> Option<u64> {
        let max = self
            .props()
            .filter_map(|(_, p)| p.group.as_deref())
            .filter_map(|g| {
                let lower = g.to_ascii_lowercase();
                let digits = lower.strip_prefix("item")?;
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                // A number too large for u64 still occupies the namespace.
                Some(digits.parse::<u64>().unwrap_or(u64::MAX))
            })
            .max()
            .unwrap_or(0);
        max.checked_add(1)
    }

    /// A stable identifier for the line at `index`: a digest of its unfolded
    /// text and how many identical lines precede it. It changes when the line
    /// changes, so an id read with an etag names exactly one line of that
    /// version.
    pub fn line_id(&self, index: usize) -> Option<String> {
        let line = self.lines.get(index)?;
        let ordinal = self.lines[..index]
            .iter()
            .filter(|l| l.text == line.text)
            .count();
        Some(sha256_hex(format!("{ordinal}\u{1f}{}", line.text))[..12].to_owned())
    }

    /// The line whose [`Card::line_id`] is `id`.
    pub fn find_id(&self, id: &str) -> Option<usize> {
        (0..self.lines.len()).find(|&i| self.line_id(i).as_deref() == Some(id))
    }

    /// The multiset of line digests that verifies a write, skipping the
    /// server-managed REV and PRODID. A grouped property's digest includes
    /// its group siblings' digests, so a label moved to another value or a
    /// dropped `itemN` grouping reads as a change, while a consistent
    /// renumbering of groups does not. Unparsed lines count by their text.
    pub fn key_counts(&self) -> BTreeMap<String, (String, u32)> {
        let mut counts: BTreeMap<String, (String, u32)> = BTreeMap::new();
        for (index, line) in self.lines.iter().enumerate() {
            let (key, name) = match &line.prop {
                Some(prop) if is_server_managed(prop) => continue,
                Some(prop) => {
                    let mut key = line_key(prop);
                    if let Some(group) = prop.group.as_deref() {
                        let mut siblings: Vec<String> = self
                            .group_members(group)
                            .filter(|(i, _)| *i != index)
                            .map(|(_, p)| line_key(p))
                            .collect();
                        siblings.sort();
                        key = sha256_hex(format!("{key}\u{1e}{}", siblings.join(",")))[..16]
                            .to_owned();
                    }
                    (key, prop.name_upper())
                }
                None if line.text.trim().is_empty() => continue,
                None => (
                    sha256_hex(format!("raw\u{1f}{}", line.text))[..16].to_owned(),
                    "(unparsed line)".to_owned(),
                ),
            };
            counts.entry(key).or_insert_with(|| (name, 0)).1 += 1;
        }
        counts
    }
}

/// REV and PRODID are rewritten by servers on every write.
pub fn is_server_managed(prop: &Prop) -> bool {
    prop.is("REV") || prop.is("PRODID")
}

/// A digest of one property: name, normalized parameters and raw value. The
/// group name is left out; [`Card::key_counts`] adds group membership.
pub fn line_key(prop: &Prop) -> String {
    let mut params: Vec<String> = prop
        .params
        .iter()
        .flat_map(|p| {
            let name = p.name.as_deref().unwrap_or("TYPE").to_ascii_uppercase();
            p.values
                .iter()
                .flat_map(|v| v.split(','))
                .map(move |v| format!("{name}={}", v.trim().to_ascii_uppercase()))
                .collect::<Vec<_>>()
        })
        .collect();
    params.sort();
    params.dedup();
    sha256_hex(format!(
        "{}\u{1f}{}\u{1f}{}",
        prop.name_upper(),
        params.join(";"),
        prop.value
    ))[..16]
        .to_owned()
}

/// Splits a content line into group, name, parameters and value. `None` for
/// text that is not a content line (kept verbatim, never edited).
fn parse_prop(text: &str) -> Option<Prop> {
    let mut in_quotes = false;
    let mut colon = None;
    let mut cuts = Vec::new();
    for (i, ch) in text.char_indices() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => cuts.push(i),
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let head_end = cuts.first().copied().unwrap_or(colon);
    let head = &text[..head_end];
    let (group, name) = match head.split_once('.') {
        Some((group, name)) => (Some(group.to_owned()), name),
        None => (None, head),
    };
    let valid =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !valid(name) || group.as_deref().is_some_and(|g| !valid(g)) {
        return None;
    }
    let mut params = Vec::new();
    let mut bounds = cuts.clone();
    bounds.push(colon);
    for pair in bounds.windows(2) {
        let raw = &text[pair[0] + 1..pair[1]];
        let mut param = parse_param(raw);
        param.raw = Some(raw.to_owned());
        params.push(param);
    }
    Some(Prop {
        group,
        name: name.to_owned(),
        params,
        value: text[colon + 1..].to_owned(),
        colon,
    })
}

fn parse_param(raw: &str) -> Param {
    let (name, values) = match raw.split_once('=') {
        Some((name, values)) => (Some(name.trim().to_owned()), values),
        None => (None, raw),
    };
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in values.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => out.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    out.push(current);
    Param {
        name,
        values: out,
        raw: None,
    }
}

/// Decodes vCard escapes: `\n` is a newline and `\x` is `x` (covers `\,`,
/// `\;`, `\\` and Apple's `\:`).
pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Splits on `sep` where it is not escaped; parts stay escaped.
pub fn split_unescaped(value: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            current.push(ch);
            if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if ch == sep {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    parts.push(current);
    parts
}

/// Escapes TEXT (RFC 6350 3.4): backslash, newline, comma and semicolon.
pub fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            other => out.push(other),
        }
    }
    out
}

/// Quotes a parameter value when it holds a separator.
fn param_value(value: &str) -> String {
    if value.contains([':', ';', ',']) {
        format!("\"{}\"", value.replace('"', ""))
    } else {
        value.to_owned()
    }
}

/// Renders a content line from its parts.
pub fn render(group: Option<&str>, name: &str, params: &[Param], value: &str) -> String {
    let mut out = String::new();
    if let Some(group) = group {
        out.push_str(group);
        out.push('.');
    }
    out.push_str(name);
    for param in params {
        out.push(';');
        if let Some(raw) = &param.raw {
            out.push_str(raw);
            continue;
        }
        if let Some(name) = &param.name {
            out.push_str(name);
            out.push('=');
        }
        let values: Vec<String> = param.values.iter().map(|v| param_value(v)).collect();
        out.push_str(&values.join(","));
    }
    out.push(':');
    out.push_str(value);
    out
}

/// Folds a line at [`FOLD_OCTETS`] without splitting a UTF-8 sequence.
pub fn fold(text: &str, eol: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 70 * 3 + 2);
    let mut width = 0;
    let mut first = true;
    for ch in text.chars() {
        let limit = if first { FOLD_OCTETS } else { FOLD_OCTETS - 1 };
        if width + ch.len_utf8() > limit {
            out.push_str(eol);
            out.push(' ');
            width = 0;
            first = false;
        }
        out.push(ch);
        width += ch.len_utf8();
    }
    out.push_str(eol);
    out
}

/// A set of line edits against one card, applied together so every index
/// refers to the original card.
#[derive(Clone, Debug, Default)]
pub struct Edits {
    /// New unfolded text for a line, or `None` to remove it.
    replace: HashMap<usize, Option<String>>,
    /// Lines inserted right after an index.
    after: BTreeMap<usize, Vec<String>>,
    /// Lines inserted before END:VCARD.
    append: Vec<String>,
}

impl Edits {
    pub fn is_touched(&self, index: usize) -> bool {
        self.replace.contains_key(&index)
    }

    pub fn is_removed(&self, index: usize) -> bool {
        matches!(self.replace.get(&index), Some(None))
    }

    /// The pending text of a line (its replacement, or the original).
    pub fn current<'a>(&'a self, card: &'a Card, index: usize) -> Option<&'a str> {
        match self.replace.get(&index) {
            Some(Some(text)) => Some(text.as_str()),
            Some(None) => None,
            None => card.lines.get(index).map(|l| l.text.as_str()),
        }
    }

    pub fn replace(&mut self, index: usize, text: String) {
        self.replace.insert(index, Some(text));
    }

    pub fn remove(&mut self, index: usize) {
        self.replace.insert(index, None);
    }

    pub fn insert_after(&mut self, index: usize, text: String) {
        self.after.entry(index).or_default().push(text);
    }

    pub fn append(&mut self, text: String) {
        self.append.push(text);
    }

    pub fn is_empty(&self) -> bool {
        self.replace.is_empty() && self.after.is_empty() && self.append.is_empty()
    }

    /// The edited card. Untouched lines keep their source bytes. A new or
    /// changed line holding a line break is refused, so no value can carry
    /// in another property.
    pub fn apply(&self, card: &Card) -> Result<Card, VcardError> {
        let mut pending = self
            .replace
            .values()
            .flatten()
            .chain(self.after.values().flatten())
            .chain(&self.append);
        if pending.any(|t| t.contains(['\r', '\n'])) {
            return Err(VcardError::LineBreak);
        }
        let end = card.end_index();
        let mut lines = Vec::with_capacity(card.lines.len() + self.append.len());
        for (index, line) in card.lines.iter().enumerate() {
            if index == end {
                lines.extend(self.append.iter().cloned().map(Line::rendered));
            }
            match self.replace.get(&index) {
                Some(Some(text)) if *text == line.text => lines.push(line.clone()),
                Some(Some(text)) => lines.push(Line::rendered(text.clone())),
                Some(None) => {}
                None => lines.push(line.clone()),
            }
            if let Some(inserted) = self.after.get(&index) {
                lines.extend(inserted.iter().cloned().map(Line::rendered));
            }
        }
        Ok(Card {
            lines,
            eol: card.eol,
        })
    }
}

/// The line's text with only its value replaced: the group, name and
/// parameter text stay exactly as written.
pub fn with_value(prop: &Prop, text: &str, value: &str) -> String {
    format!("{}:{value}", &text[..prop.colon])
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPLE: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iPhone OS 18.1//EN\r\nN:Doe;Jane;;;\r\nFN:Jane Doe\r\nitem1.EMAIL;type=INTERNET;type=pref:jane@example.com\r\nitem1.X-ABLabel:_$!<Other>!$_\r\nPHOTO;ENCODING=b;TYPE=JPEG;X-ABCROP-RECTANGLE=ABClipRect_1&0&0&10&10&abc=:/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIs\r\n IxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/2wBDAQkJCQwLDBgNDRgyIRwhMjIyMjIyMjIyMjIyMjIyMjIyMjIyMjIy\r\nX-APPLE-UNKNOWN;X-FOO=\"a:b;c\":opaque\\,value\r\nEND:VCARD\r\n";

    #[test]
    fn unedited_cards_round_trip_byte_for_byte() {
        for text in [
            APPLE,
            "BEGIN:VCARD\nVERSION:3.0\nFN:LF only\nEND:VCARD",
            "BEGIN:VCARD\r\nVERSION:3.0\r\nNOTE:mixed\nEND:VCARD\r\n",
        ] {
            let card = Card::parse(text).unwrap();
            assert_eq!(card.to_text(), text);
            assert_eq!(Edits::default().apply(&card).unwrap().to_text(), text);
        }
    }

    #[test]
    fn unfolds_and_parses_quoted_params() {
        let card = Card::parse(APPLE).unwrap();
        let photo = card.first("PHOTO").unwrap();
        assert!(photo.value.ends_with("MjIyMjIy"));
        assert!(!photo.value.contains(' '));
        let unknown = card.first("X-APPLE-UNKNOWN").unwrap();
        assert_eq!(unknown.param("X-FOO"), Some("a:b;c"));
        assert_eq!(unknown.text(), "opaque,value");
        let email = card.first("EMAIL").unwrap();
        assert_eq!(email.group.as_deref(), Some("item1"));
        assert_eq!(email.types(), vec!["INTERNET", "PREF"]);
    }

    #[test]
    fn value_edits_keep_the_prefix_and_every_other_line() {
        let card = Card::parse(APPLE).unwrap();
        let (index, email) = card.named("EMAIL").next().unwrap();
        let text = card.lines()[index].text().to_owned();
        let mut edits = Edits::default();
        edits.replace(index, with_value(email, &text, "new@example.com"));
        let out = edits.apply(&card).unwrap().to_text();
        assert_eq!(
            out,
            APPLE.replace(
                "item1.EMAIL;type=INTERNET;type=pref:jane@example.com",
                "item1.EMAIL;type=INTERNET;type=pref:new@example.com"
            )
        );
    }

    #[test]
    fn rejects_non_cards_and_multiple_cards() {
        assert_eq!(Card::parse("hello"), Err(VcardError::NotVcard));
        let two = "BEGIN:VCARD\r\nFN:a\r\nEND:VCARD\r\nBEGIN:VCARD\r\nFN:b\r\nEND:VCARD\r\n";
        assert_eq!(Card::parse(two), Err(VcardError::MultipleCards));
    }

    #[test]
    fn folds_long_lines_on_char_boundaries() {
        let text = format!("NOTE:{}", "é".repeat(60));
        let folded = fold(&text, "\r\n");
        for line in folded.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(line.len() <= 75, "{} octets", line.len());
        }
        let card = Card::parse(&format!("BEGIN:VCARD\r\n{folded}END:VCARD\r\n")).unwrap();
        assert_eq!(card.first("NOTE").unwrap().text(), "é".repeat(60));
    }

    #[test]
    fn escapes_round_trip() {
        let value = "a,b;c\\d\ne";
        assert_eq!(unescape(&escape_text(value)), value);
        assert_eq!(split_unescaped("a\\;b;c", ';'), vec!["a\\;b", "c"]);
    }

    #[test]
    fn line_ids_distinguish_identical_lines() {
        let card = Card::parse(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nEMAIL:a@example.com\r\nEMAIL:a@example.com\r\nEND:VCARD\r\n",
        )
        .unwrap();
        let first = card.line_id(2).unwrap();
        let second = card.line_id(3).unwrap();
        assert_ne!(first, second);
        assert_eq!(card.find_id(&second), Some(3));
    }
}
