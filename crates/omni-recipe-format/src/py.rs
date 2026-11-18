//! The Python standard-library behaviours the renderings were prototyped on,
//! reproduced so output stays byte-identical: `%g`, `textwrap.wrap`,
//! `difflib.get_close_matches`, `str.capitalize`, `json.dumps` and
//! `html.escape`.

use serde_json::Value;

/// `f"{x:g}"`.
pub(crate) fn fmt_g(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    let scientific = format!("{x:.5e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        strip_zeros(&format!("{x:.decimals$}")).to_owned()
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_zeros(mantissa), exponent.abs())
    }
}

fn strip_zeros(digits: &str) -> &str {
    if digits.contains('.') {
        digits.trim_end_matches('0').trim_end_matches('.')
    } else {
        digits
    }
}

/// `round(x, 1)`.
pub(crate) fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// `len(s)`.
pub(crate) fn len(s: &str) -> usize {
    s.chars().count()
}

/// `s.capitalize()`.
pub(crate) fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.as_str().to_lowercase().chars())
            .collect(),
        None => String::new(),
    }
}

/// `html.escape(s)`.
pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// `json.dumps(value, ensure_ascii=False)`.
pub(crate) fn json_dumps(value: &Value) -> String {
    let mut out = String::new();
    dump(value, &mut out);
    out
}

fn dump(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                dump(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (n, (key, item)) in map.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push_str(": ");
                dump(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

// ---------------------------------------------------------------- difflib

/// Names longer than this get no "did you mean" hint: the comparison is
/// quadratic and ids are at most 24 characters.
const CLOSE_MATCH_MAX: usize = 64;
const CLOSE_MATCH_CUTOFF: f64 = 0.6;

/// `difflib.get_close_matches(word, candidates, n=1)`: the candidate with the
/// highest Ratcliff/Obershelp ratio of at least 0.6, ties going to the greater
/// string.
pub(crate) fn closest<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let word: Vec<char> = word.chars().collect();
    if word.len() > CLOSE_MATCH_MAX {
        return None;
    }
    let mut best: Option<(f64, &'a str)> = None;
    for candidate in candidates {
        let chars: Vec<char> = candidate.chars().collect();
        if chars.len() > CLOSE_MATCH_MAX {
            continue;
        }
        let score = ratio(&chars, &word);
        if score < CLOSE_MATCH_CUTOFF {
            continue;
        }
        let better = match best {
            Some((top, name)) => score > top || (score == top && candidate > name),
            None => true,
        };
        if better {
            best = Some((score, candidate));
        }
    }
    best.map(|(_, name)| name)
}

/// `difflib.SequenceMatcher(None, a, b).ratio()`.
#[allow(clippy::cast_precision_loss)]
fn ratio(a: &[char], b: &[char]) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * matching(a, b) as f64 / total as f64
}

/// Characters in the matching blocks: the longest common run (earliest in `a`,
/// then earliest in `b`), then the same to its left and right.
fn matching(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    let (mut start_a, mut start_b, mut best) = (0, 0, 0);
    let mut previous = vec![0usize; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        let mut current = vec![0usize; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            if x == y {
                let run = previous[j] + 1;
                current[j + 1] = run;
                if run > best {
                    (start_a, start_b, best) = (i + 1 - run, j + 1 - run, run);
                }
            }
        }
        previous = current;
    }
    if best == 0 {
        return 0;
    }
    best + matching(&a[..start_a], &b[..start_b])
        + matching(&a[start_a + best..], &b[start_b + best..])
}

// --------------------------------------------------------------- textwrap

fn is_wrap_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0b' | '\x0c' | '\r' | ' ')
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_word_punct(c: char) -> bool {
    is_word(c) || matches!(c, '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

fn is_letter(c: char) -> bool {
    is_word(c) && !c.is_ascii_digit()
}

/// `textwrap.wrap(text, width, break_long_words=False)`.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let chunks = split_chunks(&munge_whitespace(text));
    let blank = |chunk: &[char]| chunk.iter().all(|c| c.is_whitespace());
    let mut lines = Vec::new();
    let mut next = 0;
    while next < chunks.len() {
        let mut line: Vec<&[char]> = Vec::new();
        let mut used = 0;
        if blank(&chunks[next]) && !lines.is_empty() {
            next += 1;
        }
        while let Some(chunk) = chunks.get(next) {
            if used + chunk.len() > width {
                break;
            }
            line.push(chunk);
            used += chunk.len();
            next += 1;
        }
        if let Some(chunk) = chunks.get(next)
            && chunk.len() > width
            && line.is_empty()
        {
            line.push(chunk);
            next += 1;
        }
        if line.last().is_some_and(|chunk| blank(chunk)) {
            line.pop();
        }
        if !line.is_empty() {
            lines.push(line.into_iter().flatten().collect());
        }
    }
    lines
}

/// Tabs expand to the next multiple of eight columns, then every whitespace
/// character becomes a space.
fn munge_whitespace(text: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let mut column = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let pad = 8 - column % 8;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            '\n' | '\r' => {
                out.push(' ');
                column = 0;
            }
            c if is_wrap_space(c) => {
                out.push(' ');
                column += 1;
            }
            c => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

/// `TextWrapper.wordsep_re.split`: runs of whitespace, em-dashes between
/// words, and words, which also end after a hyphen between letters.
fn split_chunks(text: &[char]) -> Vec<Vec<char>> {
    let mut chunks = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let start = at;
        if is_wrap_space(text[at]) {
            while at < text.len() && is_wrap_space(text[at]) {
                at += 1;
            }
        } else if let Some(end) = em_dash(text, at) {
            at = end;
        } else {
            at += 1;
            while at < text.len() {
                if text[at] == '-' && breaks_after_hyphen(text, at) {
                    at += 1;
                    break;
                }
                if is_wrap_space(text[at]) || em_dash(text, at).is_some() {
                    break;
                }
                at += 1;
            }
        }
        chunks.push(text[start..at].to_vec());
    }
    chunks
}

/// The end of a run of two or more hyphens at `at` that sits between a word
/// and a word character.
fn em_dash(text: &[char], at: usize) -> Option<usize> {
    if at == 0 || !is_word_punct(text[at - 1]) {
        return None;
    }
    let run = text[at..].iter().take_while(|c| **c == '-').count();
    let end = at + run;
    (run >= 2 && text.get(end).is_some_and(|c| is_word(*c))).then_some(end)
}

/// Whether a word may be broken after the hyphen at `at`.
fn breaks_after_hyphen(text: &[char], at: usize) -> bool {
    let letter = |i: usize| text.get(i).is_some_and(|c| is_letter(*c));
    let behind = (at >= 2 && letter(at - 2) && letter(at - 1))
        || (at >= 3 && letter(at - 3) && text[at - 2] == '-' && is_word_punct(text[at - 1]));
    let ahead =
        letter(at + 1) && (letter(at + 2) || (text.get(at + 2) == Some(&'-') && letter(at + 3)));
    behind && ahead
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn general_format_matches_python() {
        let cases = [
            (2.5, "2.5"),
            (2.0, "2"),
            (270.0, "270"),
            (0.1, "0.1"),
            (1_234_565.0, "1.23456e+06"),
            (1_000_000.0, "1e+06"),
            (999_999.0, "999999"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5, "1.5"),
            (123_456.7, "123457"),
            (-3.25, "-3.25"),
        ];
        for (x, want) in cases {
            assert_eq!(fmt_g(x), want, "{x}");
        }
    }

    #[test]
    fn rounding_to_one_decimal_is_half_even_on_the_exact_value() {
        assert_eq!(round1(0.25), 0.2);
        assert_eq!(round1(0.35), 0.3);
        assert_eq!(round1(2.675), 2.7);
        assert_eq!(round1(9.96), 10.0);
    }

    #[test]
    fn close_matches_follow_ratcliff_obershelp() {
        assert_eq!(
            closest("buter", ["butter", "oil", "dredge"]),
            Some("butter")
        );
        assert_eq!(closest("butter", ["dredge", "oil", "buter"]), Some("buter"));
        assert_eq!(closest("xyz", ["butter", "oil"]), None);
        // ratio("stalk", "stak") = 8/9 beats ratio("stick", "stak") = 6/9.
        assert_eq!(closest("stak", ["stick", "stalk"]), Some("stalk"));
        // "ab" and "ba" both score 0.5 against "a?": below the cutoff.
        assert_eq!(closest("ab", ["ba"]), None);
        // An exact tie goes to the greater string.
        assert_eq!(closest("abc", ["abd", "abe"]), Some("abe"));
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_after_hyphens_between_letters() {
        assert_eq!(
            wrap("2 tbsp flat-leaf parsley, chopped", 12),
            ["2 tbsp flat-", "leaf", "parsley,", "chopped"]
        );
        assert_eq!(wrap("a 12-inch skillet", 8), ["a", "12-inch", "skillet"]);
        assert_eq!(
            wrap("unbreakablelongword x", 5),
            ["unbreakablelongword", "x"]
        );
        assert_eq!(wrap("one--two three", 6), ["one--", "two", "three"]);
        assert_eq!(wrap("", 5), Vec::<String>::new());
        assert_eq!(wrap("  lead and trail  ", 20), ["  lead and trail"]);
    }

    #[test]
    fn json_dumps_uses_python_separators() {
        let value =
            json!({"ref": "stock", "qty": 120, "alt": [0.75, null, true], "name": "ragù \"x\""});
        assert_eq!(
            json_dumps(&value),
            r#"{"ref": "stock", "qty": 120, "alt": [0.75, null, true], "name": "ragù \"x\""}"#
        );
    }

    #[test]
    fn capitalize_lowers_the_tail() {
        assert_eq!(capitalize("tomato Sauce"), "Tomato sauce");
        assert_eq!(capitalize("élan"), "Élan");
        assert_eq!(capitalize(""), "");
    }
}
