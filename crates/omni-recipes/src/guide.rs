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

pub const VERSION: &str = "1";

const TEMPLATE: &str = include_str!("guide.md");
/// The worked example: chicken piccata as a stored document.
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
        open_adjustments: count(
            conn,
            "SELECT count(*) FROM recipes_adjustment WHERE state = 'open'",
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
        assert!(guide.contains("\"title\": \"Chicken Piccata\""));
        assert!(guide.contains("`tbsp`"));
        assert!(
            guide.contains("`course`, `cuisine`, `main`, `method`, `diet`, `occasion`, `other`")
        );
        assert!(guide.contains("made in two separate cooks"));
        assert!(guide.contains("line by line"));
        assert!(!guide.contains("{UNITS}") && !guide.contains("{EXAMPLE}"));
    }

    #[test]
    fn the_example_is_the_golden_piccata_and_validates() {
        assert_eq!(
            EXAMPLE,
            include_str!("../tests/golden/chicken-piccata.json")
        );
        let doc: omni_recipe_format::RecipeDoc = serde_json::from_str(EXAMPLE).unwrap();
        let report = omni_recipe_format::validate(&doc);
        assert!(report.errors.is_empty() && report.warnings.is_empty());
    }

    #[test]
    fn a_published_version_is_frozen() {
        // Changing the guide text means a new VERSION and a new digest here.
        assert_eq!(
            (VERSION, crate::db::sha256_hex(text().as_bytes()).as_str()),
            (
                "1",
                "245dc0c310467c6e1c089aa1d2e8528da8bb644dff4f163c2789be8127de636e"
            )
        );
    }
}
