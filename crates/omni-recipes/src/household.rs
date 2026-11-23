//! The household's standing rules, told to every agent that reads the guide,
//! fetches a page or opens an inbox item: the diet and how amounts are
//! measured. They are code, like the guide: change them here.

use omni_recipe_format::{
    Level, Problem, RecipeDoc, Report, diet_flag, diet_problems, validate, variant_docs,
};

use crate::api::{DietNote, Household};

pub const DIET: &str = "Bivalve vegetarian. No meat, poultry, fish or other seafood. Bivalves \
(mussels, clams, oysters, scallops), eggs and dairy are fine. Hidden animal products count: meat, \
poultry or fish stock and bouillon, fish sauce, anchovies and what is made with them (Caesar \
dressing, ordinary Worcestershire sauce), gelatin, lard and bacon fat. Convert a recipe to a \
natural vegetarian or bivalve version and say what changed, or tell the owner it does not fit. A \
write with such an ingredient is refused (diet_conflict) unless keepDietConflicts is passed, \
which is only for an ingredient the owner explicitly asked to keep.";

pub const MEASURING: &str = "Canada: metric, plus tsp, tbsp and cups, with a kitchen scale. Write \
each amount the way it is easiest to measure at its step: grams for anything weighed into the \
bowl (flour, sugar, butter, grated cheese, bulky things, larger liquids going into the same \
bowl), spoons in quarters and halves for small amounts (salt, spices, extracts, acids, oils under \
about 3 tbsp), counts for eggs, cloves and whole vegetables, cups or ml only where a standard \
measure is natural. Round to friendly numbers (grams to 5 or 10); small changes to the end \
amount are fine while ratios stay sensible. Off-grid amounts are reported as awkward_amount \
warnings.";

pub fn info() -> Household {
    Household {
        diet: DIET.to_owned(),
        measuring: MEASURING.to_owned(),
    }
}

/// The diet rule with the source's ingredient lines that conflict with it,
/// for a fetched page or an inbox item.
pub fn diet_note(ingredient_lines: &[String]) -> DietNote {
    DietNote {
        rule: DIET.to_owned(),
        conflicts: ingredient_lines
            .iter()
            .filter(|line| diet_flag(line).is_some())
            .cloned()
            .collect(),
    }
}

/// The format's rules and the household diet. A diet conflict blocks the
/// write unless the owner asked to keep it (`keep`) or the recipe already
/// had that ingredient (`previous`): then it is a warning, so later edits of
/// a recipe with an accepted exception are not refused.
pub fn check(doc: &RecipeDoc, previous: Option<&RecipeDoc>, keep: bool) -> Report {
    let mut report = validate(doc);
    let conflicting = |doc: &RecipeDoc| -> Vec<String> {
        doc.components
            .iter()
            .flat_map(|component| &component.ingredients)
            .filter(|ingredient| diet_flag(&ingredient.name).is_some())
            .map(|ingredient| ingredient.name.to_lowercase())
            .collect()
    };
    // What the earlier revision already had, in any of its variants too.
    let known: Vec<String> = previous
        .map(|previous| {
            let mut names = conflicting(previous);
            for (_, made) in variant_docs(previous) {
                names.extend(conflicting(&made));
            }
            names
        })
        .unwrap_or_default();
    let names: Vec<(String, String)> = doc
        .components
        .iter()
        .flat_map(|component| &component.ingredients)
        .map(|ingredient| {
            (
                format!("ingredients.{}", ingredient.id),
                ingredient.name.to_lowercase(),
            )
        })
        .collect();
    for problem in diet_problems(doc) {
        let accepted = keep
            || names
                .iter()
                .any(|(path, name)| *path == problem.path && known.contains(name));
        if accepted {
            report.warnings.push(Problem {
                level: Level::Warning,
                hint: Some("kept as an exception to the household diet".to_owned()),
                ..problem
            });
        } else {
            report.errors.push(problem);
        }
    }
    // A variant may rename an ingredient: its own conflicts count too.
    let base: Vec<(String, String)> = diet_problems(doc)
        .into_iter()
        .map(|problem| (problem.path, problem.message))
        .collect();
    for (variant, made) in variant_docs(doc) {
        for problem in diet_problems(&made) {
            if base.contains(&(problem.path.clone(), problem.message.clone())) {
                continue;
            }
            let name = made
                .components
                .iter()
                .flat_map(|component| &component.ingredients)
                .find(|ingredient| format!("ingredients.{}", ingredient.id) == problem.path)
                .map(|ingredient| ingredient.name.to_lowercase());
            let accepted = keep || name.is_some_and(|name| known.contains(&name));
            let problem = Problem {
                path: format!("variants.{}.{}", variant.id, problem.path),
                ..problem
            };
            if accepted {
                report.warnings.push(Problem {
                    level: Level::Warning,
                    hint: Some("kept as an exception to the household diet".to_owned()),
                    ..problem
                });
            } else {
                report.errors.push(problem);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use omni_recipe_format::Code;

    use super::*;

    fn piccata() -> RecipeDoc {
        serde_json::from_str(include_str!("../tests/golden/chicken-piccata.json")).unwrap()
    }

    #[test]
    fn a_new_meat_ingredient_blocks_unless_the_owner_keeps_it() {
        let doc = piccata();
        let refused = check(&doc, None, false);
        let codes: Vec<Code> = refused.errors.iter().map(|p| p.code).collect();
        assert_eq!(codes, [Code::DietConflict, Code::DietConflict]);
        let kept = check(&doc, None, true);
        assert!(kept.is_valid());
        assert_eq!(kept.warnings.len(), 2);
        assert!(kept.warnings.iter().all(|p| p.level == Level::Warning));
    }

    #[test]
    fn an_ingredient_the_recipe_already_had_does_not_block_later_edits() {
        let old = piccata();
        let mut new = old.clone();
        new.title = "Piccata".to_owned();
        assert!(check(&new, Some(&old), false).is_valid());
        new.components[0].ingredients[1].name = "anchovy fillets".to_owned();
        let report = check(&new, Some(&old), false);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].path, "ingredients.salt");
    }

    #[test]
    fn the_guide_example_fits_the_household() {
        let doc: RecipeDoc = serde_json::from_str(crate::guide::EXAMPLE).unwrap();
        let report = check(&doc, None, false);
        assert!(
            report.errors.is_empty() && report.warnings.is_empty(),
            "{report:?}"
        );
    }

    #[test]
    fn a_source_line_that_conflicts_is_listed_with_the_rule() {
        let note = diet_note(&[
            "2 cups chicken stock".to_owned(),
            "1 onion, diced".to_owned(),
            "2 tbsp fish sauce".to_owned(),
        ]);
        assert_eq!(
            note.conflicts,
            ["2 cups chicken stock", "2 tbsp fish sauce"]
        );
        assert!(note.rule.starts_with("Bivalve vegetarian"));
    }
}
