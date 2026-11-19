//! Amounts that are easy to measure. The household weighs into the bowl in
//! grams, uses spoons for small amounts and cups or millilitres where a
//! standard measure is the natural tool. A stored amount off that grid is
//! reported as a warning; a scaled amount is shown in the nearest friendly
//! form instead of an exact, awkward one.

use crate::format::{fmt_amount_exact, fmt_number_exact, fmt_unit};
use crate::index::Index;
use crate::model::RecipeDoc;
use crate::py;

/// Millilitres in a tablespoon and a cup, for moving between measures.
const TSP_PER_TBSP: f64 = 3.0;
const TBSP_PER_CUP: f64 = 16.0;
const ML_PER_TSP: f64 = 5.0;

/// Sizes cans, cartons and bottles come in: natural amounts as written.
const PACKAGE_ML: [f64; 10] = [
    156.0, 341.0, 355.0, 398.0, 473.0, 540.0, 796.0, 945.0, 946.0, 1890.0,
];
/// Standard cup measures in millilitres.
const CUP_ML: [f64; 8] = [60.0, 80.0, 120.0, 160.0, 180.0, 240.0, 360.0, 480.0];
/// More spoons than this in one go is a job for the scale or a cup.
pub const TBSP_MAX: f64 = 4.0;
const TSP_MAX: f64 = 4.5;

fn on_grid(qty: f64, step: f64) -> bool {
    let steps = qty / step;
    (steps - steps.round()).abs() < 1e-6
}

fn near(qty: f64, target: f64) -> bool {
    (qty - target).abs() < 1e-3
}

fn is_third(qty: f64) -> bool {
    let thirds = qty * 3.0;
    thirds.round() >= 1.0 && (thirds - thirds.round()).abs() < 0.04
}

fn round_to(qty: f64, step: f64) -> f64 {
    ((qty / step).round() * step).max(step)
}

fn amount(qty: f64, unit: &str) -> String {
    fmt_amount_exact(Some(qty), Some(unit))
}

/// Why `qty unit` is awkward to measure, with what to write instead, or
/// `None` when it is fine. `one_go` says the whole amount is measured at
/// once, so the size limits on spoons apply.
pub fn awkward(qty: f64, unit: &str, one_go: bool) -> Option<String> {
    if !(qty.is_finite() && qty > 0.0) {
        return None;
    }
    match unit {
        "g" => {
            // Small weights are deliberate (yeast, salt, a gum on a precise
            // scale) and stay as written.
            let step = if qty >= 20.0 {
                5.0
            } else if qty >= 10.0 {
                1.0
            } else {
                0.1
            };
            (!on_grid(qty, step))
                .then(|| format!("weigh a round number: {}", amount(round_to(qty, step), "g")))
        }
        "kg" => (!on_grid(qty * 1000.0, 5.0))
            .then(|| format!("write {}", amount(round_to(qty * 1000.0, 5.0), "g"))),
        "ml" | "l" => {
            let ml = if unit == "l" { qty * 1000.0 } else { qty };
            let fine = if ml <= 60.0 {
                on_grid(ml, 5.0)
            } else {
                on_grid(ml, 25.0)
                    || CUP_ML.iter().any(|cup| near(ml, *cup))
                    || PACKAGE_ML.iter().any(|size| near(ml, *size))
            };
            (!fine).then(|| {
                let step = if ml <= 60.0 { 5.0 } else { 25.0 };
                format!(
                    "use {}, a cup or spoon measure, or weigh it in grams",
                    amount(round_to(ml, step), "ml")
                )
            })
        }
        "tsp" => {
            if !(on_grid(qty, 0.25) || near(qty, 0.125)) {
                Some(format!(
                    "use quarter teaspoons: {}",
                    amount(round_to(qty, 0.25), "tsp")
                ))
            } else if one_go && qty > TSP_MAX {
                Some(format!(
                    "use {}",
                    amount(round_to(qty / TSP_PER_TBSP, 0.5), "tbsp")
                ))
            } else {
                None
            }
        }
        "tbsp" => {
            if !on_grid(qty, 0.5) {
                Some(format!(
                    "use half tablespoons ({}) or teaspoons ({})",
                    amount(round_to(qty, 0.5), "tbsp"),
                    amount(round_to(qty * TSP_PER_TBSP, 0.25), "tsp")
                ))
            } else if one_go && qty > TBSP_MAX {
                Some(
                    "more than 4 tbsp at once is easier weighed in grams or measured in a cup"
                        .to_owned(),
                )
            } else {
                None
            }
        }
        "cup" => {
            if on_grid(qty, 0.25) {
                None
            } else if is_third(qty) {
                // "0.33" is a rounded third: store the third itself.
                let exact = (qty * 3.0).round() / 3.0;
                ((qty - exact).abs() > 1e-4).then(|| {
                    format!(
                        "store {:.4} for {}, or weigh it in grams",
                        exact,
                        amount(exact, "cup")
                    )
                })
            } else {
                Some(
                    "use a standard cup measure (quarters or thirds), spoons, or weigh it in grams"
                        .to_owned(),
                )
            }
        }
        "oz" => Some(format!(
            "weigh it in grams: {}",
            amount(round_to(qty * 28.35, 5.0), "g")
        )),
        "lb" => Some(format!(
            "weigh it in grams: {}",
            amount(round_to(qty * 453.6, 25.0), "g")
        )),
        _ => None,
    }
}

/// One awkward amount in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Awkward {
    /// `ingredients.butter` or `steps.sear.inputs.butter`.
    pub path: String,
    pub message: String,
    pub hint: String,
}

/// Every awkward amount of `doc`: ingredient totals and the amounts steps
/// take. The source's other measure (`alt`) is the source's and is not judged.
pub fn awkward_amounts(doc: &RecipeDoc) -> Vec<Awkward> {
    let ix = Index::build(doc);
    let mut found = Vec::new();
    for ingredient in doc.components.iter().flat_map(|c| &c.ingredients) {
        let Some(unit) = ingredient.unit.as_deref() else {
            continue;
        };
        let uses = ix.consumers(&ingredient.id);
        if let Some(qty) = ingredient.qty {
            // An ingredient several steps share is never measured in one go.
            let one_go = uses.len() <= 1;
            if let Some(hint) = awkward(qty, unit, one_go) {
                found.push(Awkward {
                    path: format!("ingredients.{}", ingredient.id),
                    message: format!(
                        "{} {} is awkward to measure",
                        amount(qty, unit),
                        ingredient.name
                    ),
                    hint,
                });
            }
        }
        for used in uses {
            let Some(qty) = used.input.qty else { continue };
            let unit = used.input.unit.as_deref().unwrap_or(unit);
            if let Some(hint) = awkward(qty, unit, true) {
                found.push(Awkward {
                    path: format!("steps.{}.inputs.{}", used.step.id, ingredient.id),
                    message: format!(
                        "{} {} is awkward to measure",
                        amount(qty, unit),
                        ingredient.name
                    ),
                    hint,
                });
            }
        }
    }
    found
}

fn spoons(tsp: f64) -> String {
    // Quarter teaspoons, with an eighth as the smallest measure.
    let tsp = if tsp < 0.19 {
        0.125
    } else {
        round_to(tsp, 0.25)
    };
    let tbsp = tsp / TSP_PER_TBSP;
    if tsp >= TSP_PER_TBSP && on_grid(tbsp, 0.5) {
        return amount(tbsp, "tbsp");
    }
    if tsp > TSP_MAX {
        // Whole tablespoons and what is left in teaspoons.
        let whole = tbsp.floor();
        let left = tsp - whole * TSP_PER_TBSP;
        return format!("{} + {}", amount(whole, "tbsp"), amount(left, "tsp"));
    }
    amount(tsp, "tsp")
}

fn cups(cup: f64) -> String {
    if cup < 0.24 {
        return spoons(cup * TBSP_PER_CUP * TSP_PER_TBSP);
    }
    // The largest standard measure at or below the amount, then spoons.
    let whole = cup.floor();
    let fraction = cup - whole;
    let mut base = 0.0;
    for measure in [0.25, 1.0 / 3.0, 0.5, 2.0 / 3.0, 0.75, 1.0] {
        if measure <= fraction + 0.011 {
            base = measure;
        }
    }
    let left_tbsp = round_to_zero((fraction - base) * TBSP_PER_CUP, 0.5);
    let main = whole + base;
    if left_tbsp <= 0.0 || cup >= 2.0 {
        // Past two cups a spoonful either way is noise.
        let nearest = if cup >= 2.0 {
            round_to(cup, 0.25)
        } else {
            main
        };
        return amount(nearest, "cup");
    }
    if main <= 0.0 {
        return amount(left_tbsp, "tbsp");
    }
    format!("{} + {}", amount(main, "cup"), amount(left_tbsp, "tbsp"))
}

fn round_to_zero(qty: f64, step: f64) -> f64 {
    (qty / step).round() * step
}

fn metric(qty: f64, unit: &str) -> String {
    let rounded = if qty >= 1000.0 {
        round_to(qty, 10.0)
    } else if qty >= 50.0 {
        round_to(qty, 5.0)
    } else if qty >= 10.0 {
        qty.round_ties_even()
    } else {
        py::round1(qty)
    };
    format!(
        "{} {}",
        fmt_number_exact(rounded, Some(unit)),
        fmt_unit(rounded, Some(unit))
    )
}

/// A scaled amount in the nearest form a cook can measure: spoons in
/// quarters, cups in standard measures with spoons for what is left, grams
/// and millilitres rounded, small millilitres as spoons, and part of a
/// kilogram or litre in grams or millilitres. `None` for units this does not
/// reshape (counts, cans, pinches).
pub fn friendly(qty: f64, unit: &str) -> Option<String> {
    if !(qty.is_finite() && qty > 0.0) {
        return None;
    }
    Some(match unit {
        "tsp" => spoons(qty),
        "tbsp" => {
            if qty > TBSP_MAX && on_grid(qty / TBSP_PER_CUP, 0.25) {
                amount(qty / TBSP_PER_CUP, "cup")
            } else if on_grid(qty, 0.5) {
                amount(qty, "tbsp")
            } else {
                spoons(qty * TSP_PER_TBSP)
            }
        }
        "cup" => cups(qty),
        "g" => metric(qty, "g"),
        "kg" if qty < 1.0 => metric(qty * 1000.0, "g"),
        "ml" if qty < 30.0 && on_grid(qty, 1.25) => spoons(qty / ML_PER_TSP),
        "ml" => metric(qty, "ml"),
        "l" if qty < 1.0 => metric(qty * 1000.0, "ml"),
        _ => return None,
    })
}

/// The size of one press of a plus or minus button at `qty` of `unit`, so a
/// stepper moves between amounts a cook can measure: grams and millilitres
/// in steps that grow with the amount (and stay on the grid `awkward`
/// accepts), spoons and cups in the fractions their measures come in, and
/// anything counted by one.
fn step_size(qty: f64, unit: Option<&str>) -> (f64, f64) {
    let unit = unit.map(|unit| crate::format::unit_alias(unit).unwrap_or(unit));
    // `(from, size)`: amounts from `from` up move by `size`.
    let bands: &[(f64, f64)] = match unit {
        Some("g") => &[
            (0.0, 1.0),
            (20.0, 5.0),
            (100.0, 10.0),
            (250.0, 25.0),
            (1000.0, 50.0),
        ],
        // Millilitres are measured in fives up to 60 and in 25s above
        // (`awkward` flags anything else), so there is no step of ten.
        Some("ml") => &[(0.0, 5.0), (60.0, 25.0), (500.0, 50.0)],
        Some("kg" | "l") => &[(0.0, 0.05), (1.0, 0.1), (3.0, 0.25)],
        Some("tsp" | "cup") => &[(0.0, 0.25), (2.0, 0.5)],
        Some("tbsp") => &[(0.0, 0.5), (TBSP_MAX, 1.0)],
        Some("oz") => &[(0.0, 0.5)],
        Some("lb") => &[(0.0, 0.25)],
        // Servings, cloves, cans: whole things, and halves below one.
        _ => &[(0.0, 0.5), (1.0, 1.0)],
    };
    let (from, size) = bands
        .iter()
        .rev()
        .find(|(from, _)| qty >= *from)
        .copied()
        .unwrap_or((0.0, 1.0));
    (size, from)
}

/// `qty` after one press of a stepper's plus (`up`) or minus button: the
/// next amount on the unit's grid above or below it, so an amount off the
/// grid (a third of a cup, 326 g) snaps onto it. It never goes below one
/// step.
pub fn step(qty: f64, unit: Option<&str>, up: bool) -> f64 {
    let smallest = step_size(0.0, unit).0;
    if !(qty.is_finite() && qty > 0.0) {
        return smallest;
    }
    const EPS: f64 = 1e-6;
    let stepped = if up {
        let (size, _) = step_size(qty + EPS, unit);
        ((qty + EPS) / size).floor() * size + size
    } else {
        // The step below a boundary is the smaller one (100 g goes to 95 g),
        // and a step never jumps over the start of its own range (75 ml
        // goes to 60 ml, where the fives begin).
        let (size, from) = step_size(qty - EPS, unit);
        let below = ((qty - EPS) / size).ceil() * size - size;
        below.max(from).max(smallest)
    };
    // Sums of binary fractions of 0.05 drift; six places is far below any step.
    (stepped * 1e6).round() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_stored_amounts_pass() {
        for (qty, unit) in [
            (115.0, "g"),
            (7.0, "g"),
            (2.5, "g"),
            (0.9, "g"),
            (12.0, "g"),
            (1.2, "kg"),
            (250.0, "ml"),
            (180.0, "ml"),
            (15.0, "ml"),
            (398.0, "ml"),
            (1.5, "l"),
            (0.25, "tsp"),
            (0.125, "tsp"),
            (1.5, "tbsp"),
            (4.0, "tbsp"),
            (0.75, "cup"),
            (1.0 / 3.0, "cup"),
            (0.6667, "cup"),
            (2.0, "clove"),
            (1.0, "can"),
        ] {
            assert_eq!(awkward(qty, unit, true), None, "{qty} {unit}");
        }
    }

    #[test]
    fn awkward_stored_amounts_say_what_to_write() {
        let hint = |qty, unit| awkward(qty, unit, true).unwrap_or_default();
        assert!(hint(67.0, "ml").contains("75 ml"));
        assert!(hint(113.0, "g").contains("115 g"));
        assert!(hint(0.33, "cup").contains("0.3333"));
        assert!(hint(0.4, "cup").contains("standard cup"));
        assert!(hint(2.7, "tbsp").contains("2½ tbsp"));
        assert!(hint(0.3, "tsp").contains("¼ tsp"));
        assert!(hint(8.0, "oz").contains("225 g"));
        assert!(hint(1.0, "lb").contains("450 g"));
        assert!(hint(6.0, "tbsp").contains("weighed"));
        assert!(hint(6.0, "tsp").contains("2 tbsp"));
        assert!(hint(0.4536, "kg").contains("455 g"));
    }

    #[test]
    fn an_ingredient_shared_by_steps_may_total_more_spoons() {
        assert_eq!(awkward(5.5, "tbsp", false), None);
        assert!(awkward(5.5, "tbsp", true).is_some());
    }

    #[test]
    fn scaled_spoons_and_cups_stay_measurable() {
        let show = |qty, unit| friendly(qty, unit).unwrap_or_default();
        assert_eq!(show(0.5, "tbsp"), "½ tbsp");
        assert_eq!(show(0.25, "tbsp"), "¾ tsp");
        assert_eq!(show(0.75, "tbsp"), "2¼ tsp");
        assert_eq!(show(3.0, "tsp"), "1 tbsp");
        assert_eq!(show(4.5, "tsp"), "1½ tbsp");
        assert_eq!(show(5.0, "tsp"), "1 tbsp + 2 tsp");
        assert_eq!(show(0.1, "tsp"), "⅛ tsp");
        assert_eq!(show(0.375, "tsp"), "½ tsp");
        assert_eq!(show(8.0, "tbsp"), "½ cup");
        assert_eq!(show(6.0, "tbsp"), "6 tbsp");
        assert_eq!(show(0.125, "cup"), "2 tbsp");
        assert_eq!(show(0.375, "cup"), "⅓ cup + ½ tbsp");
        assert_eq!(show(1.125, "cup"), "1 cup + 2 tbsp");
        assert_eq!(show(0.5, "cup"), "½ cup");
        assert_eq!(show(1.0 / 6.0, "cup"), "2 tbsp + 2 tsp");
        assert_eq!(show(2.625, "cup"), "2¾ cups");
        assert_eq!(show(1.5, "cup"), "1½ cups");
    }

    #[test]
    fn scaled_metric_amounts_round_to_what_a_scale_shows() {
        let show = |qty, unit| friendly(qty, unit).unwrap_or_default();
        assert_eq!(show(172.5, "g"), "175 g");
        assert_eq!(show(57.5, "g"), "60 g");
        assert_eq!(show(22.5, "g"), "22 g");
        assert_eq!(show(3.75, "g"), "3.8 g");
        assert_eq!(show(1687.5, "g"), "1690 g");
        assert_eq!(show(0.75, "kg"), "750 g");
        assert_eq!(show(187.5, "ml"), "190 ml");
        assert_eq!(show(7.5, "ml"), "1½ tsp");
        assert_eq!(show(15.0, "ml"), "1 tbsp");
        assert_eq!(show(0.5, "l"), "500 ml");
        assert_eq!(friendly(1.5, "kg"), None);
        assert_eq!(friendly(2.0, "clove"), None);
    }

    #[test]
    fn a_stepper_moves_between_measurable_amounts_in_each_unit_family() {
        let walk = |mut qty: f64, unit: Option<&str>, up: bool, presses: usize| {
            let mut seen = Vec::new();
            for _ in 0..presses {
                qty = step(qty, unit, up);
                seen.push(qty);
            }
            seen
        };
        // Grams: ones, fives, tens, 25s, 50s by magnitude.
        assert_eq!(walk(18.0, Some("g"), true, 4), [19.0, 20.0, 25.0, 30.0]);
        assert_eq!(walk(95.0, Some("g"), true, 3), [100.0, 110.0, 120.0]);
        assert_eq!(walk(240.0, Some("g"), true, 3), [250.0, 275.0, 300.0]);
        assert_eq!(walk(1000.0, Some("g"), true, 2), [1050.0, 1100.0]);
        assert_eq!(walk(1000.0, Some("g"), false, 2), [975.0, 950.0]);
        assert_eq!(walk(100.0, Some("g"), false, 2), [95.0, 90.0]);
        assert_eq!(walk(2.0, Some("g"), false, 3), [1.0, 1.0, 1.0]);
        // An amount off the grid snaps onto it, in either direction.
        assert_eq!(step(326.0, Some("g"), true), 350.0);
        assert_eq!(step(326.0, Some("g"), false), 325.0);
        assert_eq!(step(107.0, Some("grams"), true), 110.0);
        // Millilitres: fives to 60, then 25s, then 50s; never an awkward amount.
        assert_eq!(walk(50.0, Some("ml"), true, 4), [55.0, 60.0, 75.0, 100.0]);
        assert_eq!(walk(75.0, Some("ml"), false, 3), [60.0, 55.0, 50.0]);
        assert_eq!(walk(475.0, Some("ml"), true, 3), [500.0, 550.0, 600.0]);
        assert_eq!(step(5.0, Some("ml"), false), 5.0);
        for start in [5.0, 40.0, 60.0, 130.0, 480.0, 900.0] {
            for up in [true, false] {
                let next = step(start, Some("ml"), up);
                assert_eq!(
                    awkward(next, "ml", true),
                    None,
                    "{start} ml, up {up}: {next}"
                );
            }
        }
        for start in [20.0, 45.0, 99.0, 100.0, 326.0, 990.0, 1500.0] {
            for up in [true, false] {
                let next = step(start, Some("g"), up);
                assert_eq!(awkward(next, "g", true), None, "{start} g, up {up}: {next}");
            }
        }
        // Kilograms and litres.
        assert_eq!(walk(0.9, Some("kg"), true, 3), [0.95, 1.0, 1.1]);
        assert_eq!(walk(1.0, Some("l"), false, 2), [0.95, 0.9]);
        // Spoons and cups, in the fractions their measures come in.
        assert_eq!(walk(0.25, Some("tsp"), true, 3), [0.5, 0.75, 1.0]);
        assert_eq!(walk(2.0, Some("tsp"), true, 2), [2.5, 3.0]);
        assert_eq!(walk(0.5, Some("tsp"), false, 2), [0.25, 0.25]);
        assert_eq!(walk(1.0, Some("tbsp"), true, 2), [1.5, 2.0]);
        assert_eq!(walk(4.0, Some("tbsp"), true, 2), [5.0, 6.0]);
        assert_eq!(walk(1.0, Some("tbsp"), false, 2), [0.5, 0.5]);
        assert_eq!(walk(0.5, Some("cup"), true, 3), [0.75, 1.0, 1.25]);
        assert_eq!(walk(2.0, Some("cups"), true, 2), [2.5, 3.0]);
        // A third of a cup is not on the quarter grid: the next press is.
        assert_eq!(step(0.3333, Some("cup"), true), 0.5);
        assert_eq!(step(0.3333, Some("cup"), false), 0.25);
        for (start, unit) in [(0.75, "tsp"), (1.5, "tsp"), (2.5, "tbsp"), (0.75, "cup")] {
            for up in [true, false] {
                let next = step(start, Some(unit), up);
                assert_eq!(
                    awkward(next, unit, true),
                    None,
                    "{start} {unit}, up {up}: {next}"
                );
            }
        }
        // Ounces and pounds.
        assert_eq!(walk(1.0, Some("oz"), true, 2), [1.5, 2.0]);
        assert_eq!(walk(1.0, Some("lb"), false, 2), [0.75, 0.5]);
        // Counted things go by one, whatever they are called.
        assert_eq!(walk(4.0, None, true, 2), [5.0, 6.0]);
        assert_eq!(
            walk(4.0, Some("servings"), false, 5),
            [3.0, 2.0, 1.0, 0.5, 0.5]
        );
        assert_eq!(walk(2.0, Some("clove"), false, 2), [1.0, 0.5]);
        assert_eq!(step(0.5, Some("can"), true), 1.0);
        assert_eq!(step(1.5, None, true), 2.0);
        assert_eq!(step(1.5, None, false), 1.0);
        // Nonsense in, the smallest step out.
        assert_eq!(step(0.0, Some("g"), true), 1.0);
        assert_eq!(step(f64::NAN, Some("tsp"), false), 0.25);
    }
}
