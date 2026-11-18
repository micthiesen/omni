//! `recipes_search`: SQLite FTS5 over title, ingredients, tags, step text,
//! notes and source, combined with tag, state, time and cook-history
//! filters. The agent's query never reaches FTS as written: it is cut into
//! folded words, so no input can produce FTS syntax.

use omni_recipe_format::format::fmt_amount_exact;
use omni_recipe_format::tokens;
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, params_from_iter};

use crate::api::{
    Card, CookedFilter, SearchInput, SearchOutput, SearchSort, StateFilter, UnknownTag,
};
use crate::db::{self, RECIPE_COLUMNS, doc_at, images_of, recipe_from_row, tag_name, tags_of};
use crate::error::{R, sql};
use crate::library::state_of;
use crate::service::Recipes;

/// The FTS5 `MATCH` expression for an agent's query: every word a prefix
/// term, every quoted run a phrase, all required. `None` when the query has
/// no words.
pub fn match_expression(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for (index, part) in query.split('"').enumerate() {
        let words = tokens(part);
        if words.is_empty() {
            continue;
        }
        if index % 2 == 1 {
            // Inside quotes: one phrase.
            terms.push(format!("\"{}\"", words.join(" ")));
        } else {
            terms.extend(words.into_iter().map(|word| format!("\"{word}\"*")));
        }
        if terms.len() >= 32 {
            break;
        }
    }
    terms.truncate(32);
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// A column filter: every word of `name` as a prefix term in `column`.
fn column_expression(column: &str, name: &str) -> Option<String> {
    let words = tokens(name);
    (!words.is_empty()).then(|| {
        words
            .into_iter()
            .take(8)
            .map(|word| format!("{column} : \"{word}\"*"))
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// The normalised names of a tag filter. A name that cannot be a tag is an
/// input error; one that is merely unregistered is noted in `unknown`.
fn tag_names(
    conn: &Connection,
    raw: Option<&Vec<String>>,
    unknown: &mut Vec<UnknownTag>,
) -> R<Vec<String>> {
    let mut names = Vec::new();
    for name in raw.into_iter().flatten() {
        let name = tag_name(name)?;
        let registered: i64 = conn
            .query_row(
                "SELECT count(*) FROM recipes_tag WHERE name = ?1",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if registered == 0 && !unknown.iter().any(|known| known.name == name) {
            unknown.push(UnknownTag {
                nearest: crate::tags::nearest(conn, &name)?,
                name: name.clone(),
            });
        }
        names.push(name);
    }
    Ok(names)
}

fn search(conn: &Connection, input: &SearchInput, tz: &jiff::tz::TimeZone) -> R<SearchOutput> {
    let limit = input.limit.unwrap_or(25).clamp(1, 100);
    let mut unknown_tags = Vec::new();
    let all_tags = tag_names(conn, input.tags.as_ref(), &mut unknown_tags)?;
    let no_tags = tag_names(conn, input.not_tags.as_ref(), &mut unknown_tags)?;
    let some_tags = match &input.any_tags {
        Some(any) => Some(tag_names(conn, Some(any), &mut unknown_tags)?),
        None => None,
    };
    let cursor = input.cursor.unwrap_or(0);
    let mut matches: Vec<String> = Vec::new();
    matches.extend(input.query.as_deref().and_then(match_expression));
    for ingredient in input.ingredients.iter().flatten() {
        match column_expression("ingredients", ingredient) {
            Some(expression) => matches.push(expression),
            // An ingredient with no letters matches nothing.
            None => {
                return Ok(SearchOutput {
                    total: 0,
                    next_cursor: None,
                    recipes: Vec::new(),
                    unknown_tags,
                });
            }
        }
    }
    let fts = (!matches.is_empty()).then(|| matches.join(" "));
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Sql> = Vec::new();
    let mut bind = |value: Sql| -> String {
        values.push(value);
        format!("?{}", values.len())
    };
    let from = match &fts {
        Some(expression) => {
            let slot = bind(Sql::Text(expression.clone()));
            clauses.push(format!("recipes_fts MATCH {slot}"));
            "recipes_fts JOIN recipes_recipe r ON r.id = recipes_fts.recipe_id"
        }
        None => "recipes_recipe r",
    };
    match input.state.unwrap_or(StateFilter::Active) {
        StateFilter::All => {}
        state => {
            let name = match state {
                StateFilter::Archived => "archived",
                StateFilter::Trashed => "trashed",
                _ => "active",
            };
            let slot = bind(Sql::Text(name.to_owned()));
            clauses.push(format!("r.state = {slot}"));
        }
    }
    for tag in all_tags {
        let slot = bind(Sql::Text(tag));
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM recipes_recipe_tag t WHERE t.recipe_id = r.id AND t.tag = {slot})"
        ));
    }
    for tag in no_tags {
        let slot = bind(Sql::Text(tag));
        clauses.push(format!(
            "NOT EXISTS (SELECT 1 FROM recipes_recipe_tag t WHERE t.recipe_id = r.id AND t.tag = {slot})"
        ));
    }
    if let Some(any) = some_tags {
        let slots: Vec<String> = any.into_iter().map(|tag| bind(Sql::Text(tag))).collect();
        if slots.is_empty() {
            clauses.push("0".to_owned());
        } else {
            clauses.push(format!(
                "EXISTS (SELECT 1 FROM recipes_recipe_tag t WHERE t.recipe_id = r.id AND t.tag IN ({}))",
                slots.join(", ")
            ));
        }
    }
    match input.cooked {
        Some(CookedFilter::Never) => clauses.push("r.cook_count = 0".to_owned()),
        Some(CookedFilter::Ever) => clauses.push("r.cook_count > 0".to_owned()),
        None => {}
    }
    if let Some(max) = input.max_total_min {
        let slot = bind(Sql::Integer(i64::from(max)));
        clauses.push(format!("r.total_min <= {slot}"));
    }
    if let Some(has) = input.has_image {
        clauses.push(format!(
            "{} EXISTS (SELECT 1 FROM recipes_image i WHERE i.recipe_id = r.id)",
            if has { "" } else { "NOT" }
        ));
    }
    let filter = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    let sort = input.sort.unwrap_or(if fts.is_some() {
        SearchSort::Relevance
    } else {
        SearchSort::Recent
    });
    let order = match sort {
        // Title, ingredients and tags outweigh step text and notes.
        SearchSort::Relevance if fts.is_some() => {
            "bm25(recipes_fts, 0.0, 10.0, 6.0, 6.0, 1.0, 1.0, 2.0, 0.5), r.title COLLATE NOCASE"
        }
        SearchSort::Relevance | SearchSort::Recent => {
            "max(coalesce(r.last_cooked_at, 0), r.created_at) DESC, r.id"
        }
        SearchSort::Title => "r.title COLLATE NOCASE, r.id",
        SearchSort::Added => "r.created_at DESC, r.id",
        SearchSort::LastCooked => "r.last_cooked_at IS NULL, r.last_cooked_at DESC, r.id",
        SearchSort::CookCount => "r.cook_count DESC, r.title COLLATE NOCASE, r.id",
    };
    let total: u32 = conn
        .query_row(
            &format!("SELECT count(*) FROM {from} {filter}"),
            params_from_iter(values.iter()),
            |row| row.get(0),
        )
        .map_err(sql)?;
    let snippet = if fts.is_some() {
        ", snippet(recipes_fts, -1, '', '', '…', 12)"
    } else {
        ", NULL"
    };
    let columns = RECIPE_COLUMNS
        .split(", ")
        .map(|column| format!("r.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ");
    let mut page = values.clone();
    page.push(Sql::Integer(i64::from(limit)));
    page.push(Sql::Integer(i64::from(cursor)));
    let mut statement = conn
        .prepare(&format!(
            "SELECT {columns}{snippet} FROM {from} {filter} ORDER BY {order} LIMIT ?{} OFFSET ?{}",
            page.len() - 1,
            page.len()
        ))
        .map_err(sql)?;
    let rows = statement
        .query_map(params_from_iter(page.iter()), |row| {
            Ok((recipe_from_row(row)?, row.get::<_, Option<String>>(19)?))
        })
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    let mut recipes = Vec::with_capacity(rows.len());
    for (row, snippet) in rows {
        let doc = doc_at(conn, &row.id, row.head_rev)?;
        recipes.push(Card {
            title: doc.title.clone(),
            summary: doc.summary.clone().filter(|summary| !summary.is_empty()),
            tags: tags_of(conn, &row.id)?,
            state: state_of(&row),
            r#yield: format!(
                "{} {}",
                fmt_amount_exact(Some(doc.r#yield.qty), None),
                doc.r#yield.unit
            ),
            active_min: row.active_min,
            total_min: row.total_min,
            revision: row.head_rev,
            has_image: !images_of(conn, &row.id)?.is_empty(),
            cook_count: row.cook_count,
            last_cooked_on: row.last_cooked_at.map(|at| db::local_date(at, tz)),
            open_adjustments: u32::try_from(db::open_adjustments(conn, &row.id)?.len())
                .unwrap_or(u32::MAX),
            r#match: snippet
                .map(|snippet| snippet.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|snippet| !snippet.is_empty()),
            id: row.id,
        });
    }
    Ok(SearchOutput {
        total,
        next_cursor: (cursor + limit < total).then_some(cursor + limit),
        recipes,
        unknown_tags,
    })
}

impl Recipes {
    pub async fn search(&self, input: SearchInput) -> R<SearchOutput> {
        let tz = self.tz.clone();
        self.query(move |conn| search(conn, &input, &tz)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_becomes_prefix_terms_and_phrases() {
        assert_eq!(
            match_expression("chick pic").as_deref(),
            Some("\"chick\"* \"pic\"*")
        );
        assert_eq!(
            match_expression("\"pan sauce\" lemon*").as_deref(),
            Some("\"pan sauce\" \"lemon\"*")
        );
        assert_eq!(
            match_expression("Crème Brûlée").as_deref(),
            Some("\"creme\"* \"brulee\"*")
        );
        assert_eq!(match_expression(""), None);
        assert_eq!(match_expression("  \"\" * - "), None);
    }

    #[test]
    fn hostile_queries_hold_only_words() {
        for query in [
            "NEAR(a b) OR c",
            "title : x AND NOT y",
            "\"unbalanced",
            "a\"b\"c\"",
            "(((",
            "x* ^y -z +w",
            "col:{a b}",
            "'; DROP TABLE recipes_recipe; --",
            "\u{0}\u{7}",
        ] {
            if let Some(expression) = match_expression(query) {
                assert!(
                    expression
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '"' | '*' | ' ')),
                    "{query}: {expression}"
                );
                assert_eq!(expression.matches('"').count() % 2, 0, "{expression}");
            }
        }
    }
}
