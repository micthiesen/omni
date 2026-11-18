//! schema.org `Recipe` objects in a page's `application/ld+json` scripts.
//!
//! Pages are untrusted: every field is bounded, markup inside text fields is
//! stripped, and nothing here follows a URL.

use scraper::{Html, Selector};
use serde_json::Value;

use crate::api::{Instruction, Structured};

const MAX_ITEMS: usize = 200;
const MAX_TEXT: usize = 2000;
const MAX_DEPTH: usize = 12;

/// The first Recipe object in `html`, in document order.
pub fn recipe(html: &str) -> Option<Structured> {
    let document = Html::parse_document(html);
    let selector = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;
    for script in document.select(&selector) {
        let text: String = script.text().collect();
        let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
            continue;
        };
        if let Some(found) = find(&value, 0).and_then(structured) {
            return Some(found);
        }
    }
    None
}

fn is_recipe(value: &Value) -> bool {
    match value.get("@type") {
        Some(Value::String(kind)) => type_is_recipe(kind),
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).any(type_is_recipe),
        _ => false,
    }
}

fn type_is_recipe(kind: &str) -> bool {
    kind.rsplit(['/', ':', '#']).next() == Some("Recipe")
}

/// The first Recipe object inside `value`: itself, an array member, or
/// anything nested (`@graph`, `mainEntity`).
fn find(value: &Value, depth: usize) -> Option<&Value> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value {
        Value::Array(items) => items.iter().find_map(|item| find(item, depth + 1)),
        Value::Object(map) => {
            if is_recipe(value) {
                return Some(value);
            }
            map.values()
                .filter(|inner| inner.is_object() || inner.is_array())
                .find_map(|inner| find(inner, depth + 1))
        }
        _ => None,
    }
}

/// Plain text of a field that may hold markup or entities.
fn clean(raw: &str) -> String {
    let text: String = if raw.contains('<') || raw.contains('&') {
        Html::parse_fragment(raw).root_element().text().collect()
    } else {
        raw.to_owned()
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    text.chars().take(MAX_TEXT).collect()
}

fn text_of(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => clean(text),
        Value::Number(number) => number.to_string(),
        Value::Object(map) => ["text", "name", "@value"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Value::as_str))
            .map(clean)?,
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(text_of).take(MAX_ITEMS).collect(),
        Some(other) => text_of(other).into_iter().collect(),
        None => Vec::new(),
    }
}

fn has_type(value: &Value, wanted: &str) -> bool {
    match value.get("@type") {
        Some(Value::String(kind)) => kind == wanted,
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == wanted),
        _ => false,
    }
}

/// `HowToStep` and `HowToSection` (and plain strings and `ItemList`s)
/// flattened to section and text pairs.
fn instructions(value: &Value, section: Option<&str>, depth: usize, out: &mut Vec<Instruction>) {
    if depth > MAX_DEPTH || out.len() >= MAX_ITEMS {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                instructions(item, section, depth + 1, out);
            }
        }
        Value::String(text) => {
            // One string may hold every step, one per line.
            for line in text.lines() {
                let line = clean(line);
                if !line.is_empty() && out.len() < MAX_ITEMS {
                    out.push(Instruction {
                        section: section.map(str::to_owned),
                        text: line,
                    });
                }
            }
        }
        Value::Object(map) => {
            if let Some(items) = map.get("itemListElement") {
                let name = map.get("name").and_then(Value::as_str).map(clean);
                let section = if has_type(value, "HowToSection") {
                    name.as_deref().filter(|name| !name.is_empty()).or(section)
                } else {
                    section
                };
                instructions(items, section, depth + 1, out);
            } else if let Some(text) = text_of(value) {
                out.push(Instruction {
                    section: section.map(str::to_owned),
                    text,
                });
            }
        }
        _ => {}
    }
}

/// Minutes of an ISO 8601 duration (`PT1H30M`, `P0DT20M`); seconds round up.
pub fn iso_minutes(raw: &str) -> Option<u32> {
    let text = raw.trim().to_ascii_uppercase();
    let rest = text.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((date, time)) => (date, time),
        None => (rest, ""),
    };
    let mut seconds = 0f64;
    let mut seen = false;
    let mut scan = |part: &str, units: &[(char, f64)]| -> Option<()> {
        let mut number = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' || c == ',' {
                number.push(if c == ',' { '.' } else { c });
            } else {
                let (_, factor) = units.iter().find(|(unit, _)| *unit == c)?;
                seconds += number.parse::<f64>().ok()? * factor;
                number.clear();
                seen = true;
            }
        }
        number.is_empty().then_some(())
    };
    scan(date, &[('W', 604_800.0), ('D', 86_400.0)])?;
    scan(time, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)])?;
    if !seen || !seconds.is_finite() || seconds > 60.0 * 60.0 * 24.0 * 30.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((seconds / 60.0).ceil() as u32)
}

fn minutes(recipe: &Value, key: &str) -> Option<u32> {
    recipe
        .get(key)
        .and_then(Value::as_str)
        .and_then(iso_minutes)
}

fn image_urls(value: Option<&Value>, out: &mut Vec<String>) {
    match value {
        Some(Value::String(url)) => out.push(url.trim().to_owned()),
        Some(Value::Array(items)) => {
            for item in items.iter().take(10) {
                image_urls(Some(item), out);
            }
        }
        Some(Value::Object(map)) => {
            if let Some(url) = map
                .get("url")
                .or_else(|| map.get("contentUrl"))
                .and_then(Value::as_str)
            {
                out.push(url.trim().to_owned());
            }
        }
        _ => {}
    }
}

fn structured(recipe: &Value) -> Option<Structured> {
    let ingredients = strings(
        recipe
            .get("recipeIngredient")
            .or_else(|| recipe.get("ingredients")),
    );
    let mut steps = Vec::new();
    if let Some(value) = recipe.get("recipeInstructions") {
        instructions(value, None, 0, &mut steps);
    }
    let name = recipe
        .get("name")
        .and_then(text_of)
        .or_else(|| recipe.get("headline").and_then(text_of))
        .unwrap_or_default();
    if ingredients.is_empty() && steps.is_empty() {
        return None;
    }
    let r#yield = match recipe.get("recipeYield") {
        // Sites often give [4, "4 servings"]: the one with words says more.
        Some(Value::Array(items)) => {
            let all: Vec<String> = items.iter().filter_map(text_of).collect();
            all.iter()
                .find(|item| item.chars().any(char::is_alphabetic))
                .or(all.first())
                .cloned()
        }
        Some(other) => text_of(other),
        None => None,
    };
    let author = match recipe.get("author") {
        Some(Value::Array(items)) => items.iter().find_map(text_of),
        Some(other) => text_of(other),
        None => None,
    };
    let mut images = Vec::new();
    image_urls(recipe.get("image"), &mut images);
    images.retain(|url| url.starts_with("http://") || url.starts_with("https://"));
    images.truncate(10);
    Some(Structured {
        name,
        ingredients,
        instructions: steps,
        r#yield,
        prep_min: minutes(recipe, "prepTime"),
        cook_min: minutes(recipe, "cookTime"),
        total_min: minutes(recipe, "totalTime"),
        author,
        description: recipe.get("description").and_then(text_of),
        images,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn page(json: &str) -> String {
        format!(
            "<html><head><script type=\"application/ld+json\">{json}</script></head><body></body></html>"
        )
    }

    #[test]
    fn reads_a_plain_recipe_object() {
        let found = recipe(&page(
            r#"{"@context":"https://schema.org","@type":"Recipe","name":"Lemon &amp; Ricotta Pancakes",
               "recipeYield":["12","12 pancakes"],"prepTime":"PT10M","cookTime":"PT1H5M","totalTime":"P0DT1H15M",
               "author":{"@type":"Person","name":"A. Cook"},
               "image":{"@type":"ImageObject","url":"https://example.com/p.jpg"},
               "recipeIngredient":["1 1/2 cups (190 g) all-purpose flour","1 cup <b>ricotta</b>"],
               "recipeInstructions":[{"@type":"HowToStep","text":"Whisk the flour."},
                                     {"@type":"HowToStep","name":"Cook","text":"Cook 2 minutes per side."}]}"#,
        ))
        .unwrap();
        assert_eq!(found.name, "Lemon & Ricotta Pancakes");
        assert_eq!(found.r#yield.as_deref(), Some("12 pancakes"));
        assert_eq!(
            (found.prep_min, found.cook_min, found.total_min),
            (Some(10), Some(65), Some(75))
        );
        assert_eq!(found.author.as_deref(), Some("A. Cook"));
        assert_eq!(found.images, ["https://example.com/p.jpg"]);
        assert_eq!(
            found.ingredients,
            ["1 1/2 cups (190 g) all-purpose flour", "1 cup ricotta"]
        );
        assert_eq!(found.instructions.len(), 2);
        assert_eq!(found.instructions[1].text, "Cook 2 minutes per side.");
    }

    #[test]
    fn finds_the_recipe_in_a_graph_and_flattens_sections() {
        let found = recipe(&page(
            r#"{"@context":"https://schema.org","@graph":[
                 {"@type":"WebPage","name":"Page"},
                 {"@type":["Recipe","NewsArticle"],"name":"Lasagne",
                  "image":["https://example.com/a.jpg",{"url":"https://example.com/b.jpg"},"/relative.jpg"],
                  "recipeIngredient":["500 g beef"],
                  "recipeInstructions":[
                    {"@type":"HowToSection","name":"Ragù","itemListElement":[
                       {"@type":"HowToStep","text":"Brown the beef."},"Simmer 2 hours."]},
                    {"@type":"HowToSection","name":"Assembly","itemListElement":
                       {"@type":"HowToStep","text":"Layer and bake at 375°F."}}]}]}"#,
        ))
        .unwrap();
        assert_eq!(found.name, "Lasagne");
        assert_eq!(
            found.images,
            ["https://example.com/a.jpg", "https://example.com/b.jpg"]
        );
        let flat: Vec<(Option<&str>, &str)> = found
            .instructions
            .iter()
            .map(|step| (step.section.as_deref(), step.text.as_str()))
            .collect();
        assert_eq!(
            flat,
            [
                (Some("Ragù"), "Brown the beef."),
                (Some("Ragù"), "Simmer 2 hours."),
                (Some("Assembly"), "Layer and bake at 375°F."),
            ]
        );
    }

    #[test]
    fn reads_arrays_string_instructions_and_skips_other_types() {
        let html = format!(
            "{}{}",
            page(r#"{"@type":"Article","name":"Not a recipe"}"#),
            page(
                r#"[{"@type":"BreadcrumbList"},{"@type":"Recipe","name":"Toast","image":"https://example.com/t.jpg",
                   "recipeIngredient":"2 slices bread","recipeInstructions":"Toast the bread.\nButter it."}]"#
            )
        );
        let found = recipe(&html).unwrap();
        assert_eq!(found.ingredients, ["2 slices bread"]);
        assert_eq!(found.instructions.len(), 2);
        assert!(recipe(&page(r#"{"@type":"Article"}"#)).is_none());
        assert!(recipe(&page("not json")).is_none());
        assert!(recipe("<html></html>").is_none());
    }

    #[test]
    fn durations_convert_to_minutes() {
        for (raw, minutes) in [
            ("PT20M", Some(20)),
            ("PT1H30M", Some(90)),
            ("pt2h", Some(120)),
            ("P1DT1H", Some(1500)),
            ("PT90S", Some(2)),
            ("PT0.5H", Some(30)),
            ("20 minutes", None),
            ("PT", None),
            ("P5X", None),
        ] {
            assert_eq!(iso_minutes(raw), minutes, "{raw}");
        }
    }
}
