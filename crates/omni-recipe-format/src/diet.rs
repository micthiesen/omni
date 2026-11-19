//! The household diet: bivalve vegetarian. No meat, poultry, fish or other
//! seafood; bivalves, eggs and dairy are fine. Hidden animal products count
//! (stock, fish sauce, anchovies, gelatin, lard).
//!
//! The check reads ingredient names. It is a word list, so it errs towards
//! flagging: a write that trips it by mistake is retried with the conflict
//! kept, which the owner can always ask for.

use crate::model::RecipeDoc;
use crate::validate::{Code, Level, Problem};

/// What kind of conflict an ingredient is, which decides the hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DietKind {
    Meat,
    Fish,
    Stock,
    FishSauce,
    Anchovy,
    Gelatin,
    Fat,
}

/// One ingredient name that conflicts with the diet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DietFlag {
    /// The word or phrase that matched.
    pub term: &'static str,
    pub kind: DietKind,
}

impl DietFlag {
    /// What to do instead.
    pub fn hint(self) -> &'static str {
        match self.kind {
            DietKind::Meat | DietKind::Fish => {
                "convert the dish to a vegetarian or bivalve version where that is natural \
                 (see Household in recipes_guide), or tell the owner it does not fit; pass \
                 keepDietConflicts only when the owner asked to keep this ingredient"
            }
            DietKind::Stock => {
                "use vegetable stock; pass keepDietConflicts only when the owner asked to keep it"
            }
            DietKind::FishSauce => {
                "use soy sauce or a vegan fish sauce; pass keepDietConflicts only when the owner \
                 asked to keep it"
            }
            DietKind::Anchovy => {
                "use a vegetarian (anchovy-free) version, or capers or miso for the savoury note; \
                 pass keepDietConflicts only when the owner asked to keep it"
            }
            DietKind::Gelatin => {
                "use agar or another vegetarian setting agent if the recipe still works, or a \
                 gelatin-free product; pass keepDietConflicts only when the owner asked to keep it"
            }
            DietKind::Fat => {
                "use butter or oil; pass keepDietConflicts only when the owner asked to keep it"
            }
        }
    }
}

/// A name with one of these words is a vegetarian product, whatever else it
/// says ("vegan fish sauce", "plant-based sausage", "seitan chicken").
const VEGETARIAN_WORDS: [&str; 22] = [
    "vegetarian",
    "vegan",
    "veggie",
    "veg",
    "plant-based",
    "plant based",
    "meatless",
    "meat-free",
    "meat free",
    "mock",
    "faux",
    "seitan",
    "tofu",
    "tempeh",
    "soy",
    "soya",
    "anchovy-free",
    "fish-free",
    "gelatin-free",
    "gelatine-free",
    "no-chicken",
    "no-beef",
];

/// Names that hold a flagged word and are not the animal.
const HARMLESS_PHRASES: [&str; 22] = [
    "chicken of the woods",
    "duck sauce",
    "crab apple",
    "crabapple",
    "lamb's lettuce",
    "lambs lettuce",
    "goat cheese",
    "goat's cheese",
    "goats cheese",
    "goat milk",
    "goat's milk",
    "goat yogurt",
    "goat's yogurt",
    "goat butter",
    "turkey tail",
    "lobster mushroom",
    "fish mint",
    "swedish fish",
    "coconut meat",
    "nut meat",
    "kombu dashi",
    "shiitake dashi",
];

/// Phrases checked before single words, so the hint fits.
const PHRASES: [(&str, DietKind); 16] = [
    ("fish sauce", DietKind::FishSauce),
    ("fish stock", DietKind::Stock),
    ("fish broth", DietKind::Stock),
    ("bone broth", DietKind::Stock),
    ("shrimp paste", DietKind::FishSauce),
    ("bacon fat", DietKind::Fat),
    ("bacon grease", DietKind::Fat),
    ("bacon drippings", DietKind::Fat),
    ("beef dripping", DietKind::Fat),
    ("duck fat", DietKind::Fat),
    ("chicken fat", DietKind::Fat),
    ("caesar dressing", DietKind::Anchovy),
    ("worcestershire", DietKind::Anchovy),
    ("hot dog", DietKind::Meat),
    ("sea bass", DietKind::Fish),
    ("jell-o", DietKind::Gelatin),
];

const MEAT: [&str; 45] = [
    "meat",
    "meatball",
    "beef",
    "flank steak",
    "skirt steak",
    "rump steak",
    "veal",
    "pork",
    "bacon",
    "ham",
    "prosciutto",
    "pancetta",
    "guanciale",
    "sausage",
    "chorizo",
    "salami",
    "pepperoni",
    "mortadella",
    "nduja",
    "lamb",
    "mutton",
    "goat",
    "venison",
    "bison",
    "rabbit",
    "chicken",
    "turkey",
    "duck",
    "goose",
    "quail",
    "poultry",
    "brisket",
    "oxtail",
    "sirloin",
    "ribeye",
    "drumstick",
    "giblet",
    "liverwurst",
    "bratwurst",
    "kielbasa",
    "frankfurter",
    "wiener",
    "jerky",
    "foie gras",
    "pastrami",
];

const FISH: [&str; 32] = [
    "fish",
    "salmon",
    "tuna",
    "cod",
    "halibut",
    "trout",
    "tilapia",
    "sardine",
    "mackerel",
    "haddock",
    "snapper",
    "herring",
    "pollock",
    "swordfish",
    "catfish",
    "eel",
    "shrimp",
    "prawn",
    "crab",
    "lobster",
    "crayfish",
    "crawfish",
    "squid",
    "calamari",
    "octopus",
    "seafood",
    "caviar",
    "roe",
    "bonito",
    "katsuobushi",
    "dashi",
    "surimi",
];

const SINGLE: [(&str, DietKind); 9] = [
    ("anchovy", DietKind::Anchovy),
    ("anchovies", DietKind::Anchovy),
    ("gelatin", DietKind::Gelatin),
    ("gelatine", DietKind::Gelatin),
    ("jello", DietKind::Gelatin),
    ("marshmallow", DietKind::Gelatin),
    ("lard", DietKind::Fat),
    ("suet", DietKind::Fat),
    ("tallow", DietKind::Fat),
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '-'))
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `word` is `term` or its plain plural.
fn is_term(word: &str, term: &str) -> bool {
    word == term || word.strip_suffix('s') == Some(term) || word.strip_suffix("es") == Some(term)
}

/// The conflict in an ingredient name or a source's ingredient line, if any.
pub fn diet_flag(name: &str) -> Option<DietFlag> {
    let lower = name.to_lowercase();
    let mut text = lower.clone();
    for phrase in HARMLESS_PHRASES {
        text = text.replace(phrase, " ");
    }
    let all = words(&lower);
    if all.iter().any(|word| {
        VEGETARIAN_WORDS.contains(&word.as_str()) || word == "chick'n" || word == "soyrizo"
    }) || VEGETARIAN_WORDS
        .iter()
        .any(|phrase| phrase.contains(' ') && lower.contains(phrase))
    {
        return None;
    }
    for (term, kind) in PHRASES {
        if text.contains(term) {
            return Some(DietFlag { term, kind });
        }
    }
    let words = words(&text);
    // Hyphenated words are checked whole and in parts ("chicken-style").
    let parts: Vec<&str> = words
        .iter()
        .flat_map(|word| std::iter::once(word.as_str()).chain(word.split('-')))
        .collect();
    let stockish = parts.iter().any(|word| {
        matches!(
            *word,
            "stock" | "broth" | "bouillon" | "consomme" | "consommé"
        )
    });
    for (term, kind) in SINGLE {
        if parts.iter().any(|word| is_term(word, term)) {
            return Some(DietFlag { term, kind });
        }
    }
    for (list, kind) in [(&MEAT[..], DietKind::Meat), (&FISH[..], DietKind::Fish)] {
        for term in list {
            let hit = if term.contains(' ') {
                text.contains(term)
            } else {
                parts.iter().any(|word| is_term(word, term))
            };
            if hit {
                let kind = if stockish { DietKind::Stock } else { kind };
                return Some(DietFlag { term, kind });
            }
        }
    }
    None
}

/// One `diet_conflict` problem per ingredient that conflicts with the
/// household diet, as errors. The caller decides which of them block a save.
pub fn diet_problems(doc: &RecipeDoc) -> Vec<Problem> {
    doc.components
        .iter()
        .flat_map(|component| &component.ingredients)
        .filter_map(|ingredient| {
            diet_flag(&ingredient.name).map(|flag| Problem {
                code: Code::DietConflict,
                path: format!("ingredients.{}", ingredient.id),
                message: format!(
                    "\"{}\" is not bivalve vegetarian ({})",
                    ingredient.name, flag.term
                ),
                hint: Some(flag.hint().to_owned()),
                level: Level::Error,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(name: &str) -> Option<DietKind> {
        diet_flag(name).map(|flag| flag.kind)
    }

    #[test]
    fn meat_poultry_fish_and_other_seafood_conflict() {
        for name in [
            "boneless, skinless chicken breasts",
            "lean ground beef",
            "sweet Italian sausage",
            "bacon",
            "smoked ham",
            "salmon fillets",
            "large shrimp",
            "Prawns",
            "crab meat",
            "squid rings",
            "meatballs",
            "turkey mince",
            "lamb shoulder",
            "sea bass",
        ] {
            assert!(
                matches!(kind(name), Some(DietKind::Meat | DietKind::Fish)),
                "{name}"
            );
        }
    }

    #[test]
    fn hidden_animal_products_conflict_with_a_fitting_hint() {
        assert_eq!(kind("chicken stock"), Some(DietKind::Stock));
        assert_eq!(kind("beef broth"), Some(DietKind::Stock));
        assert_eq!(kind("chicken bouillon cube"), Some(DietKind::Stock));
        assert_eq!(kind("fish stock"), Some(DietKind::Stock));
        assert_eq!(kind("fish sauce"), Some(DietKind::FishSauce));
        assert_eq!(kind("shrimp paste"), Some(DietKind::FishSauce));
        assert_eq!(kind("anchovy fillets"), Some(DietKind::Anchovy));
        assert_eq!(kind("anchovies"), Some(DietKind::Anchovy));
        assert_eq!(kind("Worcestershire sauce"), Some(DietKind::Anchovy));
        assert_eq!(kind("Caesar dressing"), Some(DietKind::Anchovy));
        assert_eq!(kind("powdered gelatin"), Some(DietKind::Gelatin));
        assert_eq!(kind("mini marshmallows"), Some(DietKind::Gelatin));
        assert_eq!(kind("lard"), Some(DietKind::Fat));
        assert_eq!(kind("bacon fat"), Some(DietKind::Fat));
        assert_eq!(kind("dashi"), Some(DietKind::Fish));
    }

    #[test]
    fn bivalves_eggs_dairy_and_plants_are_fine() {
        for name in [
            "mussels",
            "littleneck clams",
            "oysters",
            "sea scallops",
            "oyster sauce",
            "Clamato",
            "clam juice",
            "eggs",
            "unsalted butter",
            "whole milk",
            "Parmigiano-Reggiano",
            "goat cheese",
            "vegetable stock",
            "mushroom broth",
            "honey",
            "beefsteak tomatoes",
            "hen of the woods mushrooms",
            "oyster mushrooms",
            "crab apples",
            "larder",
            "graham crackers",
            "cauliflower steaks",
            "kombu dashi",
            "shallots",
            "hamburger buns, vegetarian",
        ] {
            assert_eq!(kind(name), None, "{name}");
        }
    }

    #[test]
    fn a_vegetarian_product_is_not_a_conflict() {
        for name in [
            "vegetarian Worcestershire",
            "vegan fish sauce",
            "plant-based Italian sausage",
            "veggie ground round",
            "no-chicken bouillon",
            "vegetarian chicken-style stock",
            "soy chorizo",
            "seitan",
            "smoked tofu bacon",
            "vegan marshmallows",
            "anchovy-free Worcestershire sauce",
        ] {
            assert_eq!(kind(name), None, "{name}");
        }
    }

    #[test]
    fn every_conflicting_ingredient_is_reported_with_its_path() {
        let doc: RecipeDoc =
            serde_json::from_str(include_str!("../tests/golden/chicken-piccata.json")).unwrap();
        let problems = diet_problems(&doc);
        let paths: Vec<&str> = problems.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, ["ingredients.chicken", "ingredients.stock"]);
        assert!(problems.iter().all(|p| p.code == Code::DietConflict));
        assert!(
            problems[1]
                .hint
                .as_deref()
                .unwrap()
                .contains("vegetable stock")
        );
    }
}
