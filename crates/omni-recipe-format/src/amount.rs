//! Changing one ingredient's amount: the only content edit the app makes.
//! The change is a list of ordinary edit operations, so the server writes it
//! through the same path as an agent's edit and the app can preview it.

use serde_json::{Map, Value, json};

use crate::format::{UNITS, fmt_amount_exact, is_known_unit, parse_fraction};
use crate::index::Index;
use crate::model::RecipeDoc;
use crate::ops::Op;

/// The largest quantity an amount change accepts.
pub const MAX_QTY: f64 = 100_000.0;
/// The longest unit an amount change accepts.
pub const MAX_UNIT_CHARS: usize = 16;

/// Why an amount cannot be changed. The text is shown to the cook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountError {
    NoIngredient,
    NoQuantity,
    Quantity,
    Unit,
    DividedByAmount,
}

impl std::fmt::Display for AmountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoIngredient => "the recipe no longer has this ingredient",
            Self::NoQuantity => "this ingredient has no amount to change",
            Self::Quantity => "the amount must be a number above 0, at most 100000",
            Self::Unit => "choose one of the listed units",
            Self::DividedByAmount => {
                "this ingredient is divided between steps by amount; ask an agent to change its unit"
            }
        })
    }
}

impl std::error::Error for AmountError {}

/// The units the amount sheet offers: the known ones, with `current` added
/// when the ingredient uses a unit of its own.
pub fn unit_choices(current: Option<&str>) -> Vec<String> {
    let mut units: Vec<String> = UNITS.iter().map(|unit| (*unit).to_owned()).collect();
    if let Some(current) = current
        && !is_known_unit(current)
    {
        units.push(current.to_owned());
    }
    units
}

/// A quantity that differs from a tidy number only by float noise is that
/// number, so halving and doubling returns to where it started.
fn tidy(x: f64) -> f64 {
    let rounded = (x * 10_000.0).round() / 10_000.0;
    if (x - rounded).abs() <= 1e-9 * x.abs().max(1.0) {
        rounded
    } else {
        x
    }
}

fn fields(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// The operations that set ingredient `id` to `qty` `unit` and keep the
/// document adding up. With the unit unchanged, the amounts steps take
/// explicitly and the alternative measure follow in proportion; uses given as
/// a fraction or as the rest need nothing. A new unit drops the alternative
/// measure, and is refused when steps take the ingredient by amount.
/// An unchanged amount is no operations.
pub fn amount_ops(
    doc: &RecipeDoc,
    id: &str,
    qty: f64,
    unit: Option<&str>,
) -> Result<Vec<Op>, AmountError> {
    let ix = Index::build(doc);
    let ingredient = ix.ingredient(id).ok_or(AmountError::NoIngredient)?;
    let old = ingredient.qty.ok_or(AmountError::NoQuantity)?;
    if !qty.is_finite() || qty <= 0.0 || qty > MAX_QTY {
        return Err(AmountError::Quantity);
    }
    let current = ingredient.unit.as_deref();
    if let Some(unit) = unit
        && unit.chars().count() <= MAX_UNIT_CHARS
        && (is_known_unit(unit) || Some(unit) == current)
    {
        // Allowed.
    } else if unit.is_some() {
        return Err(AmountError::Unit);
    }
    let same_unit = unit == current;
    #[allow(clippy::float_cmp)]
    if same_unit && qty == old {
        return Ok(Vec::new());
    }
    let sized: Vec<(&str, f64)> = ix
        .consumers(id)
        .iter()
        .filter_map(|used| used.input.qty.map(|qty| (used.step.id.as_str(), qty)))
        .collect();
    let mut ops = Vec::new();
    let mut set = fields(&[("qty", json!(qty))]);
    let mut unset = Vec::new();
    if same_unit {
        let ratio = qty / old;
        if let Some(alt) = &ingredient.alt {
            set.insert(
                "alt".to_owned(),
                json!({ "qty": tidy(alt.qty * ratio), "unit": alt.unit }),
            );
        }
        for (step, used) in &sized {
            // `SetInput` reaches only a step's first input of an ingredient.
            let inputs: Vec<_> = ix
                .step(step)
                .map(|step| {
                    step.inputs
                        .iter()
                        .filter(|input| input.r#ref == id)
                        .collect()
                })
                .unwrap_or_default();
            if inputs.len() != 1 {
                return Err(AmountError::DividedByAmount);
            }
            ops.push(Op::SetInput {
                step: (*step).to_owned(),
                r#ref: id.to_owned(),
                fields: fields(&[("qty", json!(tidy(used * ratio)))]),
                unset: Vec::new(),
                remove: false,
            });
        }
    } else {
        if !sized.is_empty() {
            return Err(AmountError::DividedByAmount);
        }
        match unit {
            Some(unit) => {
                set.insert("unit".to_owned(), json!(unit));
            }
            None => unset.push("unit".to_owned()),
        }
        if ingredient.alt.is_some() {
            unset.push("alt".to_owned());
        }
    }
    ops.insert(
        0,
        Op::Set {
            target: id.to_owned(),
            fields: set,
            unset,
        },
    );
    Ok(ops)
}

/// "kosher salt 2 tsp → 1 tsp": how the change reads in the revision log.
pub fn amount_summary(doc: &RecipeDoc, id: &str, qty: f64, unit: Option<&str>) -> String {
    let ix = Index::build(doc);
    let (name, before) = ix.ingredient(id).map_or_else(
        || (id.to_owned(), String::new()),
        |ingredient| {
            (
                ingredient.name.clone(),
                fmt_amount_exact(ingredient.qty, ingredient.unit.as_deref()),
            )
        },
    );
    format!("{name} {before} → {}", fmt_amount_exact(Some(qty), unit))
}

const VULGAR: [(char, &str); 9] = [
    ('½', "1/2"),
    ('¼', "1/4"),
    ('¾', "3/4"),
    ('⅓', "1/3"),
    ('⅔', "2/3"),
    ('⅛', "1/8"),
    ('⅜', "3/8"),
    ('⅝', "5/8"),
    ('⅞', "7/8"),
];

/// A typed quantity: "2", "1.5", "1,5", "1/2", "1 1/2", "1½".
pub fn parse_qty(text: &str) -> Option<f64> {
    let mut plain = String::new();
    for ch in text.trim().chars() {
        match VULGAR.iter().find(|(vulgar, _)| *vulgar == ch) {
            Some((_, fraction)) => {
                plain.push(' ');
                plain.push_str(fraction);
            }
            None => plain.push(if ch == ',' { '.' } else { ch }),
        }
    }
    let parts: Vec<&str> = plain.split_whitespace().collect();
    let value = match parts.as_slice() {
        [one] => parse_fraction(one)?,
        [whole, fraction] if fraction.contains('/') && !whole.contains(['/', '.']) => {
            parse_fraction(whole)? + parse_fraction(fraction)?
        }
        _ => return None,
    };
    (value.is_finite() && value > 0.0 && value <= MAX_QTY).then_some(value)
}

/// A quantity as the amount field shows it: at most three decimals, no
/// trailing zeros.
pub fn qty_text(qty: f64) -> String {
    let text = format!("{:.3}", tidy(qty));
    let text = text.trim_end_matches('0').trim_end_matches('.');
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_quantities_are_read() {
        assert_eq!(parse_qty("2"), Some(2.0));
        assert_eq!(parse_qty(" 1.5 "), Some(1.5));
        assert_eq!(parse_qty("1,5"), Some(1.5));
        assert_eq!(parse_qty("1/2"), Some(0.5));
        assert_eq!(parse_qty("1 1/2"), Some(1.5));
        assert_eq!(parse_qty("1½"), Some(1.5));
        assert_eq!(parse_qty("¾"), Some(0.75));
        for refused in [
            "", "0", "-1", "1/0", "abc", "1 2", "1/2 1/2", "1.5 1/2", "200000", "1e3",
        ] {
            assert_eq!(parse_qty(refused), None, "{refused}");
        }
    }

    #[test]
    fn the_field_shows_a_quantity_without_noise() {
        assert_eq!(qty_text(2.0), "2");
        assert_eq!(qty_text(1.5), "1.5");
        assert_eq!(qty_text(1.0 / 3.0), "0.333");
        assert_eq!(qty_text(0.1 + 0.2), "0.3");
        // The quick buttons multiply what the field holds.
        assert_eq!(qty_text(parse_qty("1 1/2").unwrap() * 0.5), "0.75");
        assert_eq!(qty_text(parse_qty("2").unwrap() * 1.25), "2.5");
    }
}
