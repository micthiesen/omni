//! The deterministic comparison of a document with its stored source
//! (docs/recipes.md, "Fidelity").
//!
//! It works on numbers and head words: each source ingredient line must find
//! a document ingredient with shared head words whose amount (or `alt`
//! amount) equals one the line gives, and every temperature and duration in
//! the source instructions must appear on some step. It flags some honest
//! rewrites ("a stick of butter" stored as 113 g), so it reports and never
//! blocks a save.

use std::collections::HashSet;
use std::sync::LazyLock;

use omni_recipe_format::format::{fmt_amount_exact, is_known_unit};
use omni_recipe_format::{Ingredient, RecipeDoc, tokens};
use regex::Regex;

use crate::api::{Fidelity, Review};
use crate::db::time_info;
use crate::inbox::Snapshot;

const MAX_ISSUES: usize = 40;

/// A result for a write nothing could be compared for.
pub fn unchecked(reason: &str) -> Fidelity {
    Fidelity {
        checked: false,
        reason: Some(reason.to_owned()),
        review: Review {
            ran: false,
            reason: Some(reason.to_owned()),
            ..Review::default()
        },
        ..Fidelity::default()
    }
}

/// Compares `doc` with what `snapshot` stored. Photos hold nothing a program
/// can read, so they are left to the model review.
pub fn check(snapshot: Option<&Snapshot>, doc: &RecipeDoc) -> Fidelity {
    let Some(snapshot) = snapshot else {
        return unchecked("no source");
    };
    let source = match (&snapshot.structured, &snapshot.text) {
        (Some(structured), _) if !structured.ingredients.is_empty() => Source {
            lines: structured.ingredients.clone(),
            prose: structured
                .instructions
                .iter()
                .map(|step| step.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        },
        (_, Some(text)) if snapshot.kind == "text" => split_text(text),
        _ if snapshot.kind == "photo" => return unchecked("photo source"),
        _ => return unchecked("the source has no ingredient list to compare"),
    };
    compare(&source, doc)
}

/// What a source gives to compare: its ingredient lines and the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub lines: Vec<String>,
    pub prose: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Count,
    MetricMass,
    MetricVolume,
    Spoons,
    Ounces,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Amount {
    /// In the family's base unit (g, ml, tsp, oz, or a plain count).
    base: f64,
    family: Family,
}

/// The measuring unit a word stands for, in any common spelling.
fn canonical_unit(word: &str) -> Option<&'static str> {
    Some(match word {
        "g" | "gr" | "gm" | "gms" | "gram" | "grams" => "g",
        "kg" | "kgs" | "kilo" | "kilos" | "kilogram" | "kilograms" => "kg",
        "ml" | "mls" | "millilitre" | "millilitres" | "milliliter" | "milliliters" => "ml",
        "l" | "litre" | "litres" | "liter" | "liters" => "l",
        "tsp" | "tsps" | "teaspoon" | "teaspoons" => "tsp",
        "tbsp" | "tbsps" | "tbs" | "tablespoon" | "tablespoons" => "tbsp",
        "cup" | "cups" => "cup",
        "oz" | "ounce" | "ounces" => "oz",
        "lb" | "lbs" | "pound" | "pounds" => "lb",
        _ => return None,
    })
}

fn amount(qty: f64, unit: Option<&str>) -> Amount {
    let unit = unit.map(str::to_lowercase);
    let canonical = unit.as_deref().and_then(canonical_unit);
    let (factor, family) = match canonical {
        Some("g") => (1.0, Family::MetricMass),
        Some("kg") => (1000.0, Family::MetricMass),
        Some("ml") => (1.0, Family::MetricVolume),
        Some("l") => (1000.0, Family::MetricVolume),
        Some("tsp") => (1.0, Family::Spoons),
        Some("tbsp") => (3.0, Family::Spoons),
        Some("cup") => (48.0, Family::Spoons),
        Some("oz") => (1.0, Family::Ounces),
        Some("lb") => (16.0, Family::Ounces),
        _ => (1.0, Family::Count),
    };
    Amount {
        base: qty * factor,
        family,
    }
}

fn same(a: Amount, b: Amount) -> bool {
    a.family == b.family && (a.base - b.base).abs() <= 0.02 * a.base.abs().max(b.base.abs()) + 1e-9
}

fn vulgar(c: char) -> Option<f64> {
    Some(match c {
        '¼' => 0.25,
        '½' => 0.5,
        '¾' => 0.75,
        '⅓' => 1.0 / 3.0,
        '⅔' => 2.0 / 3.0,
        '⅕' => 0.2,
        '⅙' => 1.0 / 6.0,
        '⅛' => 0.125,
        '⅜' => 0.375,
        '⅝' => 0.625,
        '⅞' => 0.875,
        _ => return None,
    })
}

static AMOUNT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?P<num>
            \d+\s+\d+\s*/\s*\d+            # 1 1/2
          | \d+\s*/\s*\d+                  # 1/2
          | \d+(?:[.,]\d+)?\s*[¼½¾⅓⅔⅕⅙⅛⅜⅝⅞]?  # 2, 2.5, 1½
          | [¼½¾⅓⅔⅕⅙⅛⅜⅝⅞]
        )
        (?:\s*(?:-|–|—|to)\s*(?:\d+(?:[.,]\d+)?|[¼½¾⅓⅔⅛⅜⅝⅞]))?   # a range: the low end counts
        \s*-?\s*
        (?P<unit>[A-Za-zµ]+\.?)?
        ",
    )
    .ok()
});

fn number(text: &str) -> Option<f64> {
    let text = text.trim();
    let mut total = 0.0;
    let mut rest = String::new();
    for c in text.chars() {
        match vulgar(c) {
            Some(value) => total += value,
            None => rest.push(c),
        }
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return (total > 0.0).then_some(total);
    }
    let parse_fraction = |part: &str| -> Option<f64> {
        let (top, bottom) = part.split_once('/')?;
        let (top, bottom): (f64, f64) = (top.trim().parse().ok()?, bottom.trim().parse().ok()?);
        (bottom != 0.0).then(|| top / bottom)
    };
    let value = if rest.contains('/') {
        match rest.split_once(char::is_whitespace) {
            Some((whole, fraction)) if fraction.contains('/') => {
                whole.trim().parse::<f64>().ok()? + parse_fraction(fraction)?
            }
            _ => parse_fraction(rest)?,
        }
    } else {
        rest.replace(',', ".").parse().ok()?
    };
    Some(total + value)
}

/// Every amount a source line states: `1 1/2 cups (190 g) flour` gives both.
fn amounts(line: &str) -> Vec<Amount> {
    let Some(regex) = AMOUNT.as_ref() else {
        return Vec::new();
    };
    regex
        .captures_iter(line)
        .filter_map(|found| {
            let qty = number(found.name("num")?.as_str())?;
            let unit = found
                .name("unit")
                .map(|unit| unit.as_str().trim_end_matches('.'));
            Some(amount(qty, unit))
        })
        .collect()
}

const STOP: &[&str] = &[
    "a",
    "an",
    "and",
    "or",
    "of",
    "the",
    "to",
    "for",
    "plus",
    "more",
    "taste",
    "into",
    "cut",
    "chopped",
    "diced",
    "minced",
    "sliced",
    "finely",
    "freshly",
    "fresh",
    "large",
    "small",
    "medium",
    "about",
    "divided",
    "optional",
    "grated",
    "peeled",
    "drained",
    "softened",
    "melted",
    "room",
    "temperature",
    "cold",
    "warm",
    "hot",
    "such",
    "as",
    "at",
    "with",
    "in",
    "on",
    "if",
    "needed",
    "pinch",
    "dash",
    "piece",
    "halved",
    "thinly",
    "roughly",
    "coarsely",
    "serving",
    "garnish",
    "each",
    "extra",
    "your",
    "choice",
    "like",
    "can",
    "cans",
    "packed",
    "lightly",
    "rinsed",
    "trimmed",
    "crushed",
    "beaten",
    "cubed",
    "shredded",
    "cooked",
    "uncooked",
    "dry",
    "dried",
    "ripe",
    "very",
    "well",
    "good",
    "quality",
    "store",
    "bought",
    "homemade",
    "inch",
    "inches",
    "cm",
    "mm",
    "x",
    "from",
    "juiced",
    "zested",
    "then",
    "until",
    "use",
];

fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies")
        && stem.len() >= 2
    {
        return format!("{stem}y");
    }
    if let Some(stem) = word.strip_suffix("oes")
        && stem.len() >= 2
    {
        return format!("{stem}o");
    }
    if let Some(stem) = word.strip_suffix("ves")
        && stem.len() >= 2
    {
        return format!("{stem}f");
    }
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_owned();
    }
    word.to_owned()
}

/// The words that name a thing: no numbers, units or prep words, singular.
fn head_words(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in tokens(text) {
        if word.chars().any(|c| c.is_ascii_digit())
            || STOP.contains(&word.as_str())
            || canonical_unit(&word).is_some()
            || is_known_unit(&word)
        {
            continue;
        }
        let word = singular(&word);
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

fn ingredient_words(ingredient: &Ingredient) -> Vec<String> {
    let mut words = head_words(&ingredient.name);
    for extra in [&ingredient.plural, &ingredient.singular]
        .into_iter()
        .flatten()
    {
        for word in head_words(extra) {
            if !words.contains(&word) {
                words.push(word);
            }
        }
    }
    words
}

fn doc_amounts(ingredient: &Ingredient) -> Vec<Amount> {
    let mut found = Vec::new();
    if let Some(qty) = ingredient.qty {
        found.push(amount(qty, ingredient.unit.as_deref()));
    }
    if let Some(alt) = &ingredient.alt {
        found.push(amount(alt.qty, Some(&alt.unit)));
    }
    found
}

fn describe(ingredient: &Ingredient) -> String {
    let amount = fmt_amount_exact(ingredient.qty, ingredient.unit.as_deref());
    if amount.is_empty() {
        ingredient.name.clone()
    } else {
        format!("{amount} {}", ingredient.name)
    }
}

static TEMP: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?x) (?P<n>\d{2,3}) \s* (?: (?:°|º|degrees?|deg\.?) \s* (?P<u>[FCfc])?\b | \s? (?P<v>[FC])\b )").ok()
});

/// `(value, unit)` of every temperature in `text`; the unit is `None` for a
/// bare degree sign.
fn temperatures(text: &str) -> Vec<(f64, Option<char>)> {
    let Some(regex) = TEMP.as_ref() else {
        return Vec::new();
    };
    regex
        .captures_iter(text)
        .filter_map(|found| {
            let value: f64 = found.name("n")?.as_str().parse().ok()?;
            let unit = found
                .name("u")
                .or_else(|| found.name("v"))
                .and_then(|unit| unit.as_str().chars().next())
                .map(|unit| unit.to_ascii_uppercase());
            Some((value, unit))
        })
        .collect()
}

static DURATION: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        (?P<a>\d+(?:\.\d+)?|[¼½¾]|\d+\s*[¼½¾])
        (?:\s*(?:-|–|—|to)\s*(?P<b>\d+(?:\.\d+)?))?
        \s*-?\s*
        (?P<u>hours?|hrs?|h|minutes?|mins?|seconds?|secs?)\b
        ",
    )
    .ok()
});

/// Every duration in `text`, in minutes; both ends of a range, and an
/// "1 hour 30 minutes" pair also as its sum.
fn durations(text: &str) -> Vec<f64> {
    let Some(regex) = DURATION.as_ref() else {
        return Vec::new();
    };
    let mut found: Vec<f64> = Vec::new();
    let mut previous: Option<(usize, f64)> = None;
    for capture in regex.captures_iter(text) {
        let Some(whole) = capture.get(0) else {
            continue;
        };
        let unit = capture.name("u").map_or("", |u| u.as_str()).to_lowercase();
        let factor = if unit.starts_with('h') {
            60.0
        } else if unit.starts_with('s') {
            1.0 / 60.0
        } else {
            1.0
        };
        let Some(low) = capture.name("a").and_then(|a| number(a.as_str())) else {
            continue;
        };
        found.push(low * factor);
        if let Some(high) = capture.name("b").and_then(|b| number(b.as_str())) {
            found.push(high * factor);
        }
        // "1 hour 30 minutes", "1 hour and 30 minutes".
        if let Some((end, hours)) = previous
            && factor == 1.0
            && matches!(text[end..whole.start()].trim(), "" | "and" | ",")
        {
            found.push(hours + low);
        }
        previous = (factor == 60.0).then_some((whole.end(), low * 60.0));
    }
    found
}

fn fmt_minutes(minutes: f64) -> String {
    if minutes < 1.0 {
        format!("{} seconds", (minutes * 60.0).round())
    } else if minutes >= 120.0 && (minutes / 60.0).fract() == 0.0 {
        format!("{} hours", minutes / 60.0)
    } else {
        format!("{} minutes", (minutes * 100.0).round() / 100.0)
    }
}

const SECTION_END: &[&str] = &[
    "instructions",
    "instruction",
    "directions",
    "direction",
    "method",
    "steps",
    "preparation",
    "procedure",
    "how to make it",
    "notes",
];

fn heading(line: &str) -> String {
    line.trim_start_matches(['#', '*', ' '])
        .trim_end_matches([':', '*', ' '])
        .to_lowercase()
}

fn is_numbered_step(line: &str) -> bool {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && digits <= 2
        && matches!(line[digits..].chars().next(), Some('.' | ')'))
        && line[digits + 1..].starts_with(' ')
}

/// The ingredient lines of pasted text and everything else. With an
/// "Ingredients" heading the section under it is the list; without one, the
/// lines that start with an amount are.
pub fn split_text(text: &str) -> Source {
    let lines: Vec<&str> = text
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(['-', '*', '•', '–', '·'])
                .trim()
        })
        .collect();
    let mut ingredient_lines: Vec<String> = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let start = lines
        .iter()
        .position(|line| matches!(heading(line).as_str(), "ingredients" | "ingredient"));
    match start {
        Some(start) => {
            prose.extend(&lines[..start]);
            let mut inside = true;
            for line in &lines[start + 1..] {
                if inside
                    && (SECTION_END.contains(&heading(line).as_str()) || is_numbered_step(line))
                {
                    inside = false;
                }
                if !inside {
                    prose.push(line);
                } else if !line.is_empty() && !line.ends_with(':') && !line.starts_with('#') {
                    ingredient_lines.push((*line).to_owned());
                }
            }
        }
        None => {
            for line in &lines {
                let starts_with_amount = line
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit() || vulgar(c).is_some());
                if starts_with_amount
                    && !is_numbered_step(line)
                    && line.split_whitespace().count() <= 14
                {
                    ingredient_lines.push((*line).to_owned());
                } else {
                    prose.push(line);
                }
            }
        }
    }
    Source {
        lines: ingredient_lines,
        prose: prose.join("\n"),
    }
}

/// The comparison itself.
pub fn compare(source: &Source, doc: &RecipeDoc) -> Fidelity {
    let ingredients: Vec<&Ingredient> = doc
        .components
        .iter()
        .flat_map(|component| &component.ingredients)
        .collect();
    let words: Vec<Vec<String>> = ingredients.iter().map(|i| ingredient_words(i)).collect();
    let mut issues: Vec<String> = Vec::new();
    let mut matched = 0u32;
    // Per document ingredient: the amounts of the source lines that named it.
    let mut claimed: Vec<Vec<(usize, Vec<Amount>)>> = vec![Vec::new(); ingredients.len()];
    for (line_index, line) in source.lines.iter().enumerate() {
        let line_words = head_words(line);
        let mut best: Option<(f64, usize)> = None;
        for (index, ingredient_words) in words.iter().enumerate() {
            let shared = ingredient_words
                .iter()
                .filter(|word| line_words.contains(word))
                .count();
            if shared == 0 || ingredient_words.is_empty() || line_words.is_empty() {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let (of_ingredient, of_line) = (
                shared as f64 / ingredient_words.len() as f64,
                shared as f64 / line_words.len() as f64,
            );
            if of_ingredient < 0.5 && of_line < 0.5 {
                continue;
            }
            let score = of_ingredient + of_line;
            if best.is_none_or(|(known, _)| score > known) {
                best = Some((score, index));
            }
        }
        match best {
            Some((_, index)) => claimed[index].push((line_index, amounts(line))),
            None => issues.push(format!("source line has no matching ingredient: {line:?}")),
        }
    }
    let prose_words: HashSet<String> = head_words(&source.prose).into_iter().collect();
    for (index, ingredient) in ingredients.iter().enumerate() {
        let lines = &claimed[index];
        if lines.is_empty() {
            // Named only in the prose ("season with salt"): still from the source.
            let named = words[index]
                .iter()
                .filter(|word| prose_words.contains(*word))
                .count();
            let in_prose = named > 0 && named * 2 >= words[index].len();
            if !in_prose {
                issues.push(format!(
                    "document ingredient is not in the source: {:?}",
                    describe(ingredient)
                ));
            }
            continue;
        }
        let stored = doc_amounts(ingredient);
        let line_ok = |amounts: &[Amount]| {
            amounts.is_empty() && stored.is_empty()
                || amounts.is_empty()
                || amounts
                    .iter()
                    .any(|given| stored.iter().any(|have| same(*given, *have)))
        };
        // A source that lists an ingredient once per use adds up to the total.
        let sums_up = lines.len() > 1
            && stored.iter().any(|have| {
                let total: f64 = lines
                    .iter()
                    .filter_map(|(_, amounts)| {
                        amounts.iter().find(|given| given.family == have.family)
                    })
                    .map(|given| given.base)
                    .sum();
                same(
                    Amount {
                        base: total,
                        family: have.family,
                    },
                    *have,
                )
            });
        for (line_index, amounts) in lines {
            if sums_up || line_ok(amounts) {
                matched += 1;
            } else {
                issues.push(format!(
                    "amount differs: the source says {:?}, the document has {:?}",
                    source.lines[*line_index],
                    describe(ingredient)
                ));
            }
        }
    }

    let steps: Vec<&omni_recipe_format::Step> = doc
        .components
        .iter()
        .flat_map(|component| &component.steps)
        .collect();
    let mut step_text = String::new();
    for step in &steps {
        for part in [Some(&step.text), step.until.as_ref(), step.tip.as_ref()]
            .into_iter()
            .flatten()
        {
            step_text.push_str(part);
            step_text.push('\n');
        }
    }
    let mut have_temps: Vec<(f64, Option<char>)> = temperatures(&step_text);
    have_temps.extend(steps.iter().filter_map(|step| {
        let temp = step.temp.as_ref()?;
        Some((
            temp.value,
            temp.unit.chars().next().map(|c| c.to_ascii_uppercase()),
        ))
    }));
    let converted = |value: f64, unit: Option<char>| match unit {
        Some('F') => Some(((value - 32.0) * 5.0 / 9.0, 'C')),
        Some('C') => Some((value * 9.0 / 5.0 + 32.0, 'F')),
        _ => None,
    };
    let mut seen: Vec<String> = Vec::new();
    for (value, unit) in temperatures(&source.prose) {
        let found = have_temps.iter().any(|(have, have_unit)| {
            let direct = (have - value).abs() < 0.5
                && (unit.is_none() || have_unit.is_none() || unit == *have_unit);
            let other = converted(value, unit).is_some_and(|(other, other_unit)| {
                (have - other).abs() <= 3.0 && *have_unit == Some(other_unit)
            });
            direct || other
        });
        let label = match unit {
            Some(unit) => format!("{value}°{unit}"),
            None => format!("{value}°"),
        };
        if !found && !seen.contains(&label) {
            issues.push(format!(
                "source temperature {label} does not appear on any step"
            ));
            seen.push(label);
        }
    }
    let time = time_info(doc);
    let mut have_minutes: Vec<f64> = durations(&step_text);
    have_minutes.extend([f64::from(time.active_min), f64::from(time.total_min)]);
    for step in &steps {
        if let Some(time) = step.time {
            have_minutes.push(f64::from(time.min));
            have_minutes.extend(time.max.map(f64::from));
        }
    }
    for minutes in durations(&source.prose) {
        let found = have_minutes
            .iter()
            .any(|have| (have - minutes).abs() < 0.01);
        let label = fmt_minutes(minutes);
        if !found && !seen.contains(&label) {
            issues.push(format!(
                "source duration of {label} does not appear on any step"
            ));
            seen.push(label);
        }
    }
    issues.truncate(MAX_ISSUES);
    Fidelity {
        checked: true,
        reason: None,
        source_ingredient_lines: Some(u32::try_from(source.lines.len()).unwrap_or(u32::MAX)),
        matched: Some(matched),
        issues,
        review: Review::default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const PICCATA: &str = include_str!("../tests/golden/chicken-piccata.json");

    const SOURCE: &str = "Chicken Piccata (serves 4)

500 g boneless, skinless chicken breasts, halved horizontally into cutlets
kosher salt and black pepper
60 g (1/2 cup) all-purpose flour, for dredging
2 tbsp olive oil
4 tbsp cold unsalted butter, divided
120 ml dry white wine
180 ml chicken stock
2 lemons
2 tbsp capers, drained (optional)
2 tbsp chopped flat-leaf parsley

1. Season the chicken and dredge in the flour, about 5 minutes.
2. Sear in the oil and 2 tbsp of the butter until golden, 3 to 4 minutes per side, 6 to 8 minutes in all.
3. Deglaze with the wine and boil until reduced by half, about 2 minutes.
4. Add the stock, the juice of 1 lemon and the capers and simmer 4-5 minutes.
5. Off the heat swirl in the rest of the butter, 1 minute.
6. Return the chicken, warm through about 1 minute, scatter with parsley and serve with lemon wedges. Takes 2 minutes.
";

    fn piccata() -> RecipeDoc {
        serde_json::from_str(PICCATA).unwrap()
    }

    fn run(text: &str, doc: &RecipeDoc) -> Fidelity {
        compare(&split_text(text), doc)
    }

    fn ingredient_mut<'a>(doc: &'a mut RecipeDoc, id: &str) -> &'a mut Ingredient {
        doc.components[0]
            .ingredients
            .iter_mut()
            .find(|ingredient| ingredient.id == id)
            .unwrap()
    }

    #[test]
    fn a_faithful_document_has_no_issues() {
        let result = run(SOURCE, &piccata());
        assert!(result.checked);
        assert_eq!(result.issues, Vec::<String>::new());
        assert_eq!(result.source_ingredient_lines, Some(9));
        assert_eq!(result.matched, Some(9));
    }

    #[test]
    fn amounts_parse_with_fractions_ranges_and_units() {
        let found = amounts("1 1/2 cups (190 g) all-purpose flour");
        assert!(same(found[0], amount(1.5, Some("cup"))));
        assert!(same(found[1], amount(190.0, Some("g"))));
        assert!(same(amounts("½ tsp salt")[0], amount(0.5, Some("tsp"))));
        assert!(same(
            amounts("1½ tablespoons oil")[0],
            amount(4.5, Some("tsp"))
        ));
        assert!(same(amounts("2-3 cloves garlic")[0], amount(2.0, None)));
        assert!(same(amounts("1kg beef")[0], amount(1000.0, Some("g"))));
        assert!(same(amounts("2 large eggs")[0], amount(2.0, None)));
        assert!(same(amount(0.5, Some("cup")), amount(8.0, Some("tbsp"))));
        assert!(!same(amount(60.0, Some("g")), amount(60.0, Some("ml"))));
    }

    // The mistakes the design's broken draft is seeded with, as they look to
    // a source comparison.

    #[test]
    fn a_split_that_does_not_add_up_shows_as_a_wrong_amount() {
        // The broken draft's butter uses add up to 3 tbsp against 4 in the source.
        let mut doc = piccata();
        ingredient_mut(&mut doc, "butter").qty = Some(3.0);
        let result = run(SOURCE, &doc);
        assert_eq!(result.issues.len(), 1, "{:?}", result.issues);
        assert!(result.issues[0].starts_with("amount differs"));
        assert!(result.issues[0].contains("4 tbsp cold unsalted butter"));
        assert!(result.issues[0].contains("3 tbsp unsalted butter"));
        assert_eq!(result.matched, Some(8));
    }

    #[test]
    fn an_ingredient_dropped_from_the_document_is_a_source_line_without_a_match() {
        // The broken draft leaves capers out of every step; an agent "fixing"
        // that by deleting the ingredient is caught here.
        let mut doc = piccata();
        doc.components[0]
            .ingredients
            .retain(|ingredient| ingredient.id != "capers");
        let result = run(SOURCE, &doc);
        assert_eq!(
            result.issues,
            ["source line has no matching ingredient: \"2 tbsp capers, drained (optional)\""]
        );
    }

    #[test]
    fn a_misread_ingredient_is_reported_from_both_sides() {
        // The broken draft's "buter": the name no longer matches its line.
        let mut doc = piccata();
        ingredient_mut(&mut doc, "butter").name = "buter".to_owned();
        let result = run(SOURCE, &doc);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.contains("no matching ingredient")
                    && issue.contains("4 tbsp cold unsalted butter")),
            "{:?}",
            result.issues
        );
        assert!(
            result
                .issues
                .contains(&"document ingredient is not in the source: \"4 tbsp buter\"".to_owned()),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn an_invented_ingredient_is_reported() {
        let mut doc = piccata();
        let mut extra = ingredient_mut(&mut doc, "capers").clone();
        extra.id = "shallot".to_owned();
        extra.name = "shallots".to_owned();
        doc.components[0].ingredients.push(extra);
        let result = run(SOURCE, &doc);
        assert_eq!(
            result.issues,
            ["document ingredient is not in the source: \"2 tbsp shallots\""]
        );
    }

    #[test]
    fn a_time_that_differs_from_the_source_is_reported() {
        // The broken draft moves steps around and leaves amounts in prose; a
        // time that changes with such a rewrite is caught from the source.
        let text = SOURCE.replace("simmer 4-5 minutes", "simmer 40-45 minutes");
        let result = run(&text, &piccata());
        assert_eq!(
            result.issues,
            [
                "source duration of 40 minutes does not appear on any step",
                "source duration of 45 minutes does not appear on any step"
            ]
        );
    }

    #[test]
    fn temperatures_match_in_either_scale() {
        let cookies: RecipeDoc =
            serde_json::from_str(include_str!("../tests/golden/chocolate-chip-cookies.json"))
                .unwrap();
        let source = |temp: &str| Source {
            lines: Vec::new(),
            prose: format!("Heat the oven to {temp}. Bake 9 to 11 minutes."),
        };
        for ok in ["375°F", "375 degrees F", "190°C", "375°", "375F"] {
            let result = compare(&source(ok), &cookies);
            assert!(
                !result
                    .issues
                    .iter()
                    .any(|issue| issue.contains("temperature")),
                "{ok}: {:?}",
                result.issues
            );
        }
        let result = compare(&source("350°F"), &cookies);
        assert!(
            result
                .issues
                .contains(&"source temperature 350°F does not appear on any step".to_owned()),
            "{:?}",
            result.issues
        );
    }

    #[test]
    fn durations_read_ranges_hours_and_pairs() {
        assert_eq!(durations("simmer 4-5 minutes"), [4.0, 5.0]);
        assert_eq!(durations("3 to 4 min per side"), [3.0, 4.0]);
        assert_eq!(durations("cook 2.5 hours"), [150.0]);
        assert_eq!(durations("1 hour 30 minutes"), [60.0, 30.0, 90.0]);
        assert_eq!(durations("rest 30 seconds"), [0.5]);
        assert_eq!(durations("serves 4, makes 12"), Vec::<f64>::new());
    }

    #[test]
    fn an_ingredients_heading_bounds_the_list() {
        let source = split_text(
            "My Soup\n\n## Ingredients\n- 2 carrots\n- salt, to taste\nFor the top:\n- 1 tbsp cream\n\n## Method\n1. Boil 10 minutes.\n2 more notes",
        );
        assert_eq!(
            source.lines,
            ["2 carrots", "salt, to taste", "1 tbsp cream"]
        );
        assert!(source.prose.contains("Boil 10 minutes"));
        assert!(source.prose.contains("2 more notes"));
    }

    #[test]
    fn a_source_that_lists_one_ingredient_per_use_adds_up() {
        let text = SOURCE.replace(
            "4 tbsp cold unsalted butter, divided",
            "2 tbsp unsalted butter\n2 tbsp cold unsalted butter",
        );
        let result = run(&text, &piccata());
        assert_eq!(result.issues, Vec::<String>::new());
        assert_eq!(result.matched, Some(10));
    }

    #[test]
    fn photos_and_missing_sources_are_not_checked() {
        let doc = piccata();
        let none = check(None, &doc);
        assert!(!none.checked);
        assert_eq!(none.reason.as_deref(), Some("no source"));
        let photo = Snapshot {
            kind: "photo".to_owned(),
            photos: vec!["a".repeat(64)],
            ..Snapshot::default()
        };
        let result = check(Some(&photo), &doc);
        assert!(!result.checked);
        assert_eq!(result.reason.as_deref(), Some("photo source"));
    }
}
