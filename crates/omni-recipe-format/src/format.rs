//! How amounts, units, times and temperatures read.

use crate::model::{Ingredient, Temp};
use crate::py;

pub const METRIC_UNITS: [&str; 4] = ["g", "kg", "ml", "l"];

/// Every unit the validator accepts without a warning, sorted.
pub const UNITS: [&str; 19] = [
    "batch", "bunch", "can", "clove", "cup", "g", "kg", "l", "lb", "ml", "oz", "piece", "pinch",
    "slice", "sprig", "stalk", "stick", "tbsp", "tsp",
];

const UNIT_ALIASES: [(&str, &str); 20] = [
    ("grams", "g"),
    ("gram", "g"),
    ("kilograms", "kg"),
    ("millilitres", "ml"),
    ("milliliters", "ml"),
    ("litre", "l"),
    ("liter", "l"),
    ("teaspoon", "tsp"),
    ("teaspoons", "tsp"),
    ("tablespoon", "tbsp"),
    ("tablespoons", "tbsp"),
    ("cups", "cup"),
    ("ounce", "oz"),
    ("ounces", "oz"),
    ("pound", "lb"),
    ("pounds", "lb"),
    ("cloves", "clove"),
    ("stalks", "stalk"),
    ("sprigs", "sprig"),
    ("pinches", "pinch"),
];

const VULGAR: [(f64, &str); 6] = [
    (1.0 / 8.0, "⅛"),
    (1.0 / 4.0, "¼"),
    (1.0 / 3.0, "⅓"),
    (1.0 / 2.0, "½"),
    (2.0 / 3.0, "⅔"),
    (3.0 / 4.0, "¾"),
];

pub fn is_metric(unit: &str) -> bool {
    METRIC_UNITS.contains(&unit)
}

pub fn is_known_unit(unit: &str) -> bool {
    UNITS.contains(&unit)
}

/// The listed unit a common spelling stands for: "grams" is "g".
pub fn unit_alias(unit: &str) -> Option<&'static str> {
    UNIT_ALIASES
        .iter()
        .find(|(alias, _)| *alias == unit)
        .map(|(_, known)| *known)
}

/// The listed unit `unit` most likely means, for the `unknown_unit` hint.
pub fn nearest_unit(unit: &str) -> Option<&'static str> {
    unit_alias(unit).or_else(|| py::closest(unit, UNITS))
}

fn present(unit: Option<&str>) -> Option<&str> {
    unit.filter(|unit| !unit.is_empty())
}

/// A quantity for display: plain rounding for metric units (`270`), vulgar
/// fractions for spoons, cups and counts (`1⅛`).
pub fn fmt_number(x: f64, unit: Option<&str>) -> String {
    if unit.is_some_and(is_metric) {
        let rounded = if x >= 100.0 {
            (x / 5.0).round_ties_even() * 5.0
        } else if x >= 10.0 {
            x.round_ties_even()
        } else {
            py::round1(x)
        };
        return py::fmt_g(rounded);
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut whole = (x + 1e-9) as i64;
    #[allow(clippy::cast_precision_loss)]
    let fraction = x - whole as f64;
    let mut best = (0.0, "");
    for candidate in VULGAR.into_iter().chain([(1.0, "")]) {
        if (candidate.0 - fraction).abs() < (best.0 - fraction).abs() {
            best = candidate;
        }
    }
    if (best.0 - fraction).abs() > 0.04 {
        return format!("{x:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
    }
    if best.0 == 1.0 {
        whole = whole.saturating_add(1);
    }
    if best.1.is_empty() {
        whole.to_string()
    } else if whole == 0 {
        best.1.to_owned()
    } else {
        format!("{whole}{}", best.1)
    }
}

/// The unit as it reads after `qty`: count units follow the number.
pub fn fmt_unit(qty: f64, unit: Option<&str>) -> String {
    let Some(unit) = present(unit) else {
        return String::new();
    };
    if is_metric(unit) || matches!(unit, "tsp" | "tbsp" | "oz" | "lb") {
        unit.to_owned()
    } else if qty > 1.0 + 1e-9 {
        match unit {
            "pinch" => "pinches".to_owned(),
            other => format!("{other}s"),
        }
    } else {
        unit.to_owned()
    }
}

/// `2 tbsp`, `1½ cups`, `3`, or nothing for an unmeasured amount.
pub fn fmt_amount(qty: Option<f64>, unit: Option<&str>) -> String {
    let Some(qty) = qty else {
        return String::new();
    };
    let number = fmt_number(qty, unit);
    let unit = fmt_unit(qty, unit);
    if unit.is_empty() {
        number
    } else {
        format!("{number} {unit}")
    }
}

/// Count nouns follow the quantity: "2 lemons" at half scale is "1 lemon".
/// Irregular nouns use the ingredient's `plural` and `singular`.
pub fn inflect(
    name: &str,
    qty: Option<f64>,
    unit: Option<&str>,
    ingredient: &Ingredient,
) -> String {
    let Some(qty) = qty else {
        return name.to_owned();
    };
    if present(unit).is_some() {
        return name.to_owned();
    }
    if (qty - 1.0).abs() < 1e-9 {
        let singular = ingredient.singular.as_deref().filter(|s| !s.is_empty());
        if let Some(singular) = singular
            && ingredient.plural.as_deref() == Some(name)
        {
            return singular.to_owned();
        }
        return match name.strip_suffix('s') {
            Some(stem) if !stem.ends_with('s') => stem.to_owned(),
            _ => name.to_owned(),
        };
    }
    if qty > 1.0 && !name.ends_with('s') {
        return match &ingredient.plural {
            Some(plural) => plural.clone(),
            None => format!("{name}s"),
        };
    }
    name.to_owned()
}

fn fmt_minutes(minutes: u32) -> String {
    if minutes >= 120 && minutes.is_multiple_of(30) {
        format!("{} h", py::fmt_g(f64::from(minutes) / 60.0))
    } else if minutes >= 90 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// A duration or range in minutes, compact: `8 min`, `4–5 min`, `2.5 h`.
pub fn fmt_time(min: u32, max: Option<u32>) -> String {
    match max {
        Some(max) if max != min && max >= 120 => {
            format!("{} to {}", fmt_minutes(min), fmt_minutes(max))
        }
        Some(max) if max != min => format!("{min}–{max} min"),
        _ => fmt_minutes(min),
    }
}

/// [`fmt_time`] with the units spelled out for step text: `4–5 minutes`.
pub fn fmt_time_prose(min: u32, max: Option<u32>) -> String {
    let compact = fmt_time(min, max);
    let mut words: Vec<&str> = Vec::new();
    for word in compact.split(' ') {
        let one = words.last() == Some(&"1");
        words.push(match (word, one) {
            ("min", true) => "minute",
            ("min", false) => "minutes",
            ("h", true) => "hour",
            ("h", false) => "hours",
            (other, _) => other,
        });
    }
    words.join(" ")
}

/// `375°F (190°C)` in text; the source's scale alone where space is short.
pub fn fmt_temp(temp: &Temp, both: bool) -> String {
    let value = fmt_plain(temp.value);
    let unit = temp.unit.as_str();
    if !both {
        return format!("{value}°{unit}");
    }
    let fahrenheit = unit == "F";
    let other = if fahrenheit {
        ((temp.value - 32.0) * 5.0 / 9.0 / 5.0).round_ties_even() * 5.0
    } else {
        ((temp.value * 9.0 / 5.0 + 32.0) / 5.0).round_ties_even() * 5.0
    };
    let other_unit = if fahrenheit { 'C' } else { 'F' };
    format!("{value}°{unit} ({}°{other_unit})", fmt_plain(other))
}

/// A scale factor for display: `1.5`, `2`.
pub fn fmt_factor(factor: f64) -> String {
    py::fmt_g(factor)
}

fn fmt_plain(x: f64) -> String {
    if x == 0.0 {
        "0".to_owned()
    } else {
        x.to_string()
    }
}

/// The value of a fraction string: `"2/3"`, `"0.5"`.
pub fn parse_fraction(text: &str) -> Option<f64> {
    let text = text.trim();
    let value = match text.split_once('/') {
        Some((numerator, denominator)) => {
            let numerator: u32 = numerator.trim().parse().ok()?;
            let denominator: u32 = denominator.trim().parse().ok()?;
            if denominator == 0 {
                return None;
            }
            f64::from(numerator) / f64::from(denominator)
        }
        None => {
            if !text.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                return None;
            }
            text.parse().ok()?
        }
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(name: &str) -> Ingredient {
        serde_json::from_value(serde_json::json!({"id": "x", "name": name})).unwrap()
    }

    #[test]
    fn metric_amounts_round_plainly() {
        assert_eq!(fmt_number(270.0, Some("ml")), "270");
        assert_eq!(fmt_number(272.4, Some("ml")), "270");
        assert_eq!(fmt_number(262.5, Some("g")), "260");
        assert_eq!(fmt_number(267.5, Some("g")), "270");
        assert_eq!(fmt_number(12.5, Some("g")), "12");
        assert_eq!(fmt_number(13.5, Some("g")), "14");
        assert_eq!(fmt_number(2.25, Some("kg")), "2.2");
        assert_eq!(fmt_number(1.5, Some("l")), "1.5");
        assert_eq!(fmt_number(1_000_000.0, Some("g")), "1e+06");
    }

    #[test]
    fn spoons_cups_and_counts_use_vulgar_fractions() {
        assert_eq!(fmt_number(1.125, Some("cup")), "1⅛");
        assert_eq!(fmt_number(0.5, Some("cup")), "½");
        assert_eq!(fmt_number(0.75, None), "¾");
        assert_eq!(fmt_number(2.0, None), "2");
        assert_eq!(fmt_number(1.98, Some("tsp")), "2");
        assert_eq!(fmt_number(0.2, Some("tsp")), "0.2");
        assert_eq!(fmt_number(1.4, None), "1.4");
        assert_eq!(fmt_number(2.0 / 3.0, None), "⅔");
        assert_eq!(fmt_number(10.0, None), "10");
        assert_eq!(fmt_number(1.06, None), "1.06");
    }

    #[test]
    fn count_units_follow_the_number() {
        assert_eq!(fmt_amount(Some(1.0), Some("pinch")), "1 pinch");
        assert_eq!(fmt_amount(Some(2.0), Some("pinch")), "2 pinches");
        assert_eq!(fmt_amount(Some(1.5), Some("cup")), "1½ cups");
        assert_eq!(fmt_amount(Some(3.0), Some("tbsp")), "3 tbsp");
        assert_eq!(fmt_amount(Some(2.0), None), "2");
        assert_eq!(fmt_amount(None, Some("g")), "");
    }

    #[test]
    fn count_nouns_follow_the_quantity() {
        let lemons = count("lemons");
        assert_eq!(inflect("lemons", Some(1.0), None, &lemons), "lemon");
        assert_eq!(inflect("lemons", Some(1.5), None, &lemons), "lemons");
        assert_eq!(inflect("onion", Some(2.0), None, &count("onion")), "onions");
        assert_eq!(inflect("glass", Some(1.0), None, &count("glass")), "glass");
        assert_eq!(
            inflect("flour", Some(2.0), Some("g"), &count("flour")),
            "flour"
        );
        let mut leaves = count("bay leaves");
        leaves.plural = Some("bay leaves".to_owned());
        leaves.singular = Some("bay leaf".to_owned());
        assert_eq!(inflect("bay leaves", Some(1.0), None, &leaves), "bay leaf");
        let mut leaf = count("bay leaf");
        leaf.plural = Some("bay leaves".to_owned());
        assert_eq!(inflect("bay leaf", Some(2.0), None, &leaf), "bay leaves");
    }

    #[test]
    fn times_read_compactly_and_in_prose() {
        assert_eq!(fmt_time(8, None), "8 min");
        assert_eq!(fmt_time(4, Some(5)), "4–5 min");
        assert_eq!(fmt_time(150, None), "2.5 h");
        assert_eq!(fmt_time(100, None), "1 h 40 min");
        assert_eq!(fmt_time(265, None), "4 h 25 min");
        assert_eq!(fmt_time(60, Some(120)), "60 min to 2 h");
        assert_eq!(fmt_time(5, Some(5)), "5 min");
        assert_eq!(fmt_time_prose(1, None), "1 minute");
        assert_eq!(fmt_time_prose(2, None), "2 minutes");
        assert_eq!(fmt_time_prose(4, Some(5)), "4–5 minutes");
        assert_eq!(fmt_time_prose(150, None), "2.5 hours");
        assert_eq!(fmt_time_prose(100, None), "1 hour 40 minutes");
        assert_eq!(fmt_time_prose(120, Some(180)), "2 hours to 3 hours");
    }

    #[test]
    fn temperatures_show_both_scales_in_text() {
        let f = Temp {
            value: 375.0,
            unit: "F".to_owned(),
        };
        assert_eq!(fmt_temp(&f, true), "375°F (190°C)");
        assert_eq!(fmt_temp(&f, false), "375°F");
        let c = Temp {
            value: 180.0,
            unit: "C".to_owned(),
        };
        assert_eq!(fmt_temp(&c, true), "180°C (355°F)");
        let sous_vide = Temp {
            value: 62.5,
            unit: "C".to_owned(),
        };
        assert_eq!(fmt_temp(&sous_vide, true), "62.5°C (145°F)");
    }

    #[test]
    fn fractions_parse_as_ratios_or_decimals() {
        assert_eq!(parse_fraction("2/3"), Some(2.0 / 3.0));
        assert_eq!(parse_fraction(" 1/2 "), Some(0.5));
        assert_eq!(parse_fraction("0.25"), Some(0.25));
        assert_eq!(parse_fraction("1/0"), None);
        assert_eq!(parse_fraction("half"), None);
        assert_eq!(parse_fraction("-1/2"), None);
        assert_eq!(parse_fraction("inf"), None);
        assert_eq!(parse_fraction(""), None);
    }

    #[test]
    fn unknown_units_resolve_to_the_nearest_listed_one() {
        assert_eq!(nearest_unit("grams"), Some("g"));
        assert_eq!(nearest_unit("cupz"), Some("cup"));
        assert_eq!(nearest_unit("handful"), None);
    }
}
