//! The authoring guide `recipes_guide` returns: the rules, the format
//! reference, a worked example, the units and the live tag vocabulary. Tool
//! descriptions stay short and point here.
//!
//! The text is versioned: change it by bumping [`VERSION`] and the digest in
//! the test, so a change to what agents are told is always deliberate.

use omni_recipe_format::format::UNITS;
use rusqlite::Connection;

use crate::api::{GuideOutput, LibraryCounts, TagGroup};
use crate::error::{R, sql};
use crate::service::Recipes;
use crate::tags;

pub const VERSION: &str = "7";

const TEMPLATE: &str = include_str!("guide.md");
/// The worked example: cauliflower piccata as a stored document.
pub const EXAMPLE: &str = include_str!("guide_example.json");

/// The guide's Markdown.
pub fn text() -> String {
    let groups = TagGroup::ALL
        .iter()
        .map(|group| format!("`{}`", group.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let units = UNITS
        .iter()
        .map(|unit| format!("`{unit}`"))
        .collect::<Vec<_>>()
        .join(" ");
    TEMPLATE
        .replace("{UNITS}", &units)
        .replace("{GROUPS}", &groups)
        .replace("{EXAMPLE}", EXAMPLE.trim_end())
}

fn count(conn: &Connection, statement: &str) -> R<u32> {
    conn.query_row(statement, [], |row| row.get(0)).map_err(sql)
}

pub fn counts(conn: &Connection) -> R<LibraryCounts> {
    Ok(LibraryCounts {
        active: count(
            conn,
            "SELECT count(*) FROM recipes_recipe WHERE state = 'active'",
        )?,
        archived: count(
            conn,
            "SELECT count(*) FROM recipes_recipe WHERE state = 'archived'",
        )?,
        trashed: count(
            conn,
            "SELECT count(*) FROM recipes_recipe WHERE state = 'trashed'",
        )?,
        never_cooked: count(
            conn,
            "SELECT count(*) FROM recipes_recipe WHERE state = 'active' AND cook_count = 0",
        )?,
        inbox_pending: count(
            conn,
            "SELECT count(*) FROM recipes_inbox WHERE state = 'pending'",
        )?,
        active_cooks: count(
            conn,
            "SELECT count(*) FROM recipes_cook WHERE state = 'active'",
        )?,
    })
}

impl Recipes {
    pub async fn guide(&self) -> R<GuideOutput> {
        self.query(|conn| {
            Ok(GuideOutput {
                version: VERSION.to_owned(),
                guide: text(),
                units: UNITS.iter().map(|unit| (*unit).to_owned()).collect(),
                tag_groups: TagGroup::ALL.to_vec(),
                tags: tags::registry(conn)?,
                library: counts(conn)?,
                household: crate::household::info(),
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_guide_carries_the_rules_the_example_and_the_vocabulary() {
        let guide = text();
        for rule in 1..=10 {
            assert!(guide.contains(&format!("\n{rule}. ")), "rule {rule}");
        }
        assert!(guide.contains("\"title\": \"Cauliflower Piccata\""));
        assert!(guide.contains("`tbsp`"));
        assert!(
            guide.contains("`course`, `cuisine`, `main`, `method`, `diet`, `occasion`, `other`")
        );
        assert!(guide.contains("### Diet: bivalve vegetarian"));
        assert!(guide.contains("### Amounts that are easy to measure"));
        assert!(guide.contains("line by line"));
        assert!(guide.contains("## Requests from the app"));
        assert!(guide.contains("`recipes_requests_list`") && guide.contains("`resultRevision`"));
        assert!(!guide.contains("{UNITS}") && !guide.contains("{EXAMPLE}"));
    }

    #[test]
    fn the_example_follows_the_guide_it_illustrates() {
        let doc: omni_recipe_format::RecipeDoc = serde_json::from_str(EXAMPLE).unwrap();
        let report = crate::household::check(&doc, None, false);
        assert!(report.errors.is_empty() && report.warnings.is_empty());
        // The features the guide says it shows.
        assert!(EXAMPLE.contains("\"rest\": true") && EXAMPLE.contains("\"part\": \"juice\""));
        assert!(EXAMPLE.contains("\"byproducts\"") && EXAMPLE.contains("\"toTaste\": true"));
    }

    /// Every key of the document format is named in the guide's reference,
    /// so a field added to the format cannot go undocumented.
    #[test]
    fn the_reference_names_every_field_of_the_format() {
        fn keys(schema: &serde_json::Value, into: &mut Vec<String>) {
            match schema {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::Object(properties)) = map.get("properties") {
                        into.extend(properties.keys().cloned());
                    }
                    map.values().for_each(|value| keys(value, into));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|item| keys(item, into)),
                _ => {}
            }
        }
        let settings = schemars::generate::SchemaSettings::draft2020_12()
            .with(|settings| settings.inline_subschemas = true);
        let schema = settings
            .into_generator()
            .into_root_schema_for::<omni_recipe_format::RecipeDoc>()
            .to_value();
        let mut fields = Vec::new();
        keys(&schema, &mut fields);
        fields.sort();
        fields.dedup();
        assert!(fields.len() > 40, "{fields:?}");
        let guide = text();
        let reference =
            &guide[guide.find("## Format reference").unwrap()..guide.find("## Units").unwrap()];
        let words: Vec<&str> = reference
            .split(|c: char| !c.is_ascii_alphanumeric())
            .collect();
        let missing: Vec<&String> = fields
            .iter()
            .filter(|field| !words.contains(&field.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "the guide's reference omits {missing:?}"
        );
    }

    #[test]
    fn a_published_version_is_frozen() {
        // Changing the guide text means a new VERSION and a new digest here.
        assert_eq!(
            (VERSION, crate::db::sha256_hex(text().as_bytes()).as_str()),
            (
                "7",
                "daa741f1aa49d7b198892b8ef8c35ceb7b378e5f4a517929f296987493cfe387"
            )
        );
    }
}
