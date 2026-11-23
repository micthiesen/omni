//! Numbers written into prose that go stale when the recipe is scaled:
//! "Divide the dough into 8 pieces" still says 8 at double the recipe.
//! Times, temperatures, sizes, ratios, repeats and measured amounts (which
//! `literal_quantity` reports) do not count.

use crate::text::placeholders;

/// A number in prose followed by the word it counts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unscaled {
    /// As written: "8", "2–3".
    pub shown: String,
    /// The first number's value.
    pub value: f64,
    /// The counted word, lowercased: "pieces".
    pub word: String,
}

/// Words after a number that say it stays true at any scale.
const STABLE_AFTER: &[&str] = &[
    // Time.
    "s", "sec", "secs", "second", "seconds", "min", "mins", "minute", "minutes", "h", "hr", "hrs",
    "hour", "hours", "day", "days", "week", "weeks", "month", "months", "night", "nights",
    // Temperature.
    "degree", "degrees", "deg", "f", "c", // Size, of the food or of a pot or tin.
    "mm", "cm", "m", "in", "inch", "inches", "ft", "foot", "feet", "x", "by", "quart", "quarts",
    "qt", "cavity", "cavities", // Ratios, repeats and parts of one item.
    "percent", "part", "parts", "times", "time", "turn", "turns", "fold", "folds", "pulse",
    "pulses", "stroke", "strokes", "side", "sides", "more", "half", "halves", "thirds", "quarters",
    "st", "nd", "rd", "th", // Words that do not name what is counted.
    "and", "or", "with", "to", "of", "the", "a", "an", "at", "on", "for", "from", "until", "into",
    "per", "each", "then", "as", "if", "is", "are", "plus", "minus",
];

/// Measuring words, left to literal_quantity.
const MEASURES: &[&str] = &[
    "g",
    "kg",
    "ml",
    "l",
    "tsp",
    "tbsp",
    "cup",
    "cups",
    "grams",
    "gram",
    "teaspoon",
    "teaspoons",
    "tablespoon",
    "tablespoons",
    "oz",
    "ounce",
    "ounces",
    "lb",
    "lbs",
    "pound",
    "pounds",
    "litre",
    "litres",
    "liter",
    "liters",
    "millilitre",
    "millilitres",
    "milliliter",
    "milliliters",
    "pint",
    "pints",
    "gallon",
    "gallons",
    "tbsps",
    "tsps",
    "kgs",
    "pinch",
    "pinches",
];

/// Words that describe what is counted rather than name it: "8 medium
/// potatoes" counts potatoes.
const MODIFIERS: &[&str] = &[
    "small", "medium", "large", "big", "extra", "whole", "heaping", "level", "generous", "thin",
    "thick", "fresh", "baking", "loaf", "round", "square", "equal", "even",
];

/// Words before a number that make it a label: "step 3", "gas mark 4".
const LABEL_BEFORE: [&str; 11] = [
    "step", "steps", "speed", "setting", "mark", "level", "number", "no", "page", "power",
    "position",
];

/// Characters right after a number that make it a ratio, temperature, size or
/// ordinal.
fn stable_mark(c: char) -> bool {
    matches!(
        c,
        '%' | '°' | 'º' | '˚' | ':' | '×' | '"' | '\'' | '″' | '/'
    )
}

fn is_vulgar(c: char) -> bool {
    matches!(c, '¼' | '½' | '¾' | '⅓' | '⅔' | '⅛')
}

fn vulgar_value(c: char) -> f64 {
    match c {
        '¼' => 0.25,
        '½' => 0.5,
        '¾' => 0.75,
        '⅓' => 1.0 / 3.0,
        '⅔' => 2.0 / 3.0,
        _ => 0.125,
    }
}

/// Prose with every `{placeholder}` blanked out, so neither ids nor labels
/// are read as numbers.
fn without_placeholders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for placeholder in placeholders(text) {
        out.push_str(&text[at..placeholder.start]);
        out.push_str(" · ");
        at = placeholder.end;
    }
    out.push_str(&text[at..]);
    out
}

/// A number at `at` in `chars`: digits with an optional `.` or `/` part, a
/// digit run with a vulgar fraction ("1½"), or one vulgar fraction. Returns
/// its end and value.
fn number(chars: &[char], at: usize) -> Option<(usize, f64)> {
    let first = *chars.get(at)?;
    if is_vulgar(first) {
        return Some((at + 1, vulgar_value(first)));
    }
    let mut end = at;
    while chars.get(end).is_some_and(char::is_ascii_digit) {
        end += 1;
    }
    if end == at {
        return None;
    }
    let whole: String = chars[at..end].iter().collect();
    let mut value: f64 = whole.parse().ok()?;
    if let Some(&c) = chars.get(end)
        && is_vulgar(c)
    {
        return Some((end + 1, value + vulgar_value(c)));
    }
    if chars.get(end) == Some(&'.') && chars.get(end + 1).is_some_and(char::is_ascii_digit) {
        let mut tail = end + 1;
        while chars.get(tail).is_some_and(char::is_ascii_digit) {
            tail += 1;
        }
        let written: String = chars[at..tail].iter().collect();
        value = written.parse().ok()?;
        end = tail;
    }
    Some((end, value))
}

fn word_at(chars: &[char], at: usize) -> (usize, String) {
    let mut end = at;
    while chars.get(end).is_some_and(|c| c.is_alphabetic()) {
        end += 1;
    }
    (
        end,
        chars[at..end].iter().collect::<String>().to_lowercase(),
    )
}

fn skip_space(chars: &[char], mut at: usize) -> usize {
    while chars.get(at).is_some_and(|c| *c == ' ' || *c == '\u{a0}') {
        at += 1;
    }
    at
}

/// The word before `at`, lowercased.
fn word_before(chars: &[char], at: usize) -> String {
    let mut end = at;
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && chars[start - 1].is_alphabetic() {
        start -= 1;
    }
    chars[start..end].iter().collect::<String>().to_lowercase()
}

/// Every number in `text` that counts something and would read the same at
/// any scale.
pub(crate) fn unscaled_numbers(text: &str) -> Vec<Unscaled> {
    let prose = without_placeholders(text);
    let chars: Vec<char> = prose.chars().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let follows_word = at > 0
            && chars.get(at - 1).is_some_and(|c| {
                c.is_alphanumeric() || matches!(c, '_' | '.' | ',' | '/' | ':' | '×')
            });
        let Some((mut end, value)) = (!follows_word).then(|| number(&chars, at)).flatten() else {
            at += 1;
            continue;
        };
        let start = at;
        // A range: "2-3", "2 to 3", "2 or 3".
        let joined = skip_space(&chars, end);
        let (after_join, join) = word_at(&chars, joined);
        let dash = matches!(chars.get(joined), Some('-' | '–'));
        let range_start = if dash {
            Some(skip_space(&chars, joined + 1))
        } else if join == "to" || join == "or" {
            Some(skip_space(&chars, after_join))
        } else {
            None
        };
        if let Some(second) = range_start
            && let Some((range_end, _)) = number(&chars, second)
        {
            end = range_end;
        }
        if chars.get(end).copied().is_some_and(stable_mark) {
            at = end;
            continue;
        }
        // "9-inch": a hyphen joins the number to its word.
        let mut next = if chars.get(end) == Some(&'-') {
            end + 1
        } else {
            end
        };
        next = skip_space(&chars, next);
        if chars.get(next).copied().is_some_and(stable_mark) {
            at = end;
            continue;
        }
        let (after_word, mut word) = word_at(&chars, next);
        if MODIFIERS.contains(&word.as_str()) {
            let (_, noun) = word_at(&chars, skip_space(&chars, after_word));
            if !noun.is_empty() {
                word = format!("{word} {noun}");
            }
        }
        let before = word_before(&chars, start);
        if word.is_empty()
            || STABLE_AFTER.contains(&word.as_str())
            || MEASURES.contains(&word.as_str())
            || LABEL_BEFORE.contains(&before.as_str())
        {
            at = end;
            continue;
        }
        found.push(Unscaled {
            shown: chars[start..end]
                .iter()
                .collect::<String>()
                .trim()
                .to_owned(),
            value,
            word,
        });
        at = end;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> Vec<String> {
        unscaled_numbers(text)
            .iter()
            .map(|hit| format!("{} {}", hit.shown, hit.word))
            .collect()
    }

    #[test]
    fn counts_that_go_stale_are_found() {
        assert_eq!(shown("Divide the dough into 8 pieces."), ["8 pieces"]);
        assert_eq!(
            shown("Fry in 2 batches; makes 24 cookies."),
            ["2 batches", "24 cookies"]
        );
        assert_eq!(shown("Shape into 10-12 balls"), ["10-12 balls"]);
        assert_eq!(shown("Add 2 cloves garlic"), ["2 cloves"]);
        assert_eq!(
            shown("Line 2 baking sheets; about 8 medium potatoes"),
            ["2 baking sheets", "8 medium potatoes"]
        );
    }

    #[test]
    fn times_temperatures_sizes_ratios_and_labels_are_left_alone() {
        for text in [
            "Bake 3 to 4 minutes per side.",
            "Rest 1 hour, then 30 min more.",
            "Heat to 180°C or 350 F.",
            "Roll 3 mm thick in a 23 x 33 cm tin.",
            "Use a 9-inch pan.",
            "A 1:1 mix, 70% hydration.",
            "Fold 3 times, turning 2 times.",
            "Set the mixer to speed 4; gas mark 6; see step 2.",
            "Cut each in 2 halves.",
            "The 2nd tray.",
            "Add 2 tbsp butter and 1½ cups milk.",
            "Divide among {bowls|4 bowls}.",
            "Version 2.0",
            "Serves 4.",
            "Brown it in a 4 quart pot, 1 part sugar to 6 parts flour.",
            "Pulse 16 to 20 pulses; soak 13 and 12 hours apart.",
            "A 12 cavity muffin tin and 2 pints of stock.",
            "Heat to 350ºF or 180˚C, 200 deg; 2 tbsps and 2 pinches.",
            "Repeat steps 3 and 4 until firm; use 2 of them.",
        ] {
            assert_eq!(shown(text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn a_vulgar_fraction_or_decimal_reads_its_value() {
        let hits = unscaled_numbers("about 1½ loaves and 2.5 trays");
        let values: Vec<f64> = hits.iter().map(|hit| hit.value).collect();
        assert_eq!(values, [1.5, 2.5]);
    }
}
