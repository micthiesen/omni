//! The library screen's decisions: what a recipe is found by, which recipes
//! a search and tag filter leave, and their order.

use std::collections::{BTreeMap, BTreeSet};

use omni_api::common::Ms;
use omni_api::fond::{RecipeRecord, RecipeState, Tag};
use omni_recipe_format::{RecipeDoc, matches, search_text, tokens};

/// The tag groups in the order the filter sheet lists them.
pub const GROUPS: [(&str, &str); 7] = [
    ("course", "Course"),
    ("cuisine", "Cuisine"),
    ("main", "Main ingredient"),
    ("method", "Method"),
    ("diet", "Diet"),
    ("occasion", "Occasion"),
    ("other", "Other"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// The later of last cooked and added, newest first.
    #[default]
    Recent,
    Title,
    MostCooked,
    Newest,
}

impl Sort {
    pub const ALL: [Sort; 4] = [Sort::Recent, Sort::Title, Sort::MostCooked, Sort::Newest];

    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Recent => "recent",
            Sort::Title => "title",
            Sort::MostCooked => "cooked",
            Sort::Newest => "newest",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "Recent",
            Sort::Title => "Title",
            Sort::MostCooked => "Most cooked",
            Sort::Newest => "Newest",
        }
    }

    pub fn parse(text: &str) -> Sort {
        Sort::ALL
            .into_iter()
            .find(|sort| sort.as_str() == text)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    pub query: String,
    pub tags: BTreeSet<String>,
    /// Also list archived recipes.
    pub archived: bool,
}

/// What the library needs of one recipe.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub state: RecipeState,
    pub tags: Vec<String>,
    pub cook_count: u32,
    pub last_cooked_at: Option<Ms>,
    pub created_at: Ms,
    /// Folded words of everything the recipe is found by.
    pub words: Vec<String>,
}

/// The words a recipe is found by: the document's search text (title,
/// ingredient names, source names, step text), its tags and its notes.
pub fn search_words(
    doc: Option<&RecipeDoc>,
    title: &str,
    tags: &[String],
    notes: &str,
) -> Vec<String> {
    let mut words = match doc {
        Some(doc) => tokens(&search_text(doc)),
        None => tokens(title),
    };
    for tag in tags {
        words.extend(tokens(tag));
    }
    words.extend(tokens(notes));
    words.sort();
    words.dedup();
    words
}

pub fn item(record: &RecipeRecord, doc: Option<&RecipeDoc>, title: &str) -> Item {
    Item {
        id: record.id.clone(),
        title: title.to_owned(),
        state: record.state,
        tags: record.tags.clone(),
        cook_count: record.cook_count,
        last_cooked_at: record.last_cooked_at,
        created_at: record.created_at,
        words: search_words(doc, title, &record.tags, &record.notes),
    }
}

fn in_scope(item: &Item, archived: bool) -> bool {
    match item.state {
        RecipeState::Active => true,
        RecipeState::Archived => archived,
        RecipeState::Trashed => false,
    }
}

/// Any of the selected tags within a group, all of the groups that have a
/// selection. A selected tag the registry does not know forms its own group.
fn tags_match(item: &Item, selected: &BTreeSet<String>, groups: &BTreeMap<String, String>) -> bool {
    let mut by_group: BTreeMap<&str, bool> = BTreeMap::new();
    for tag in selected {
        let group = groups.get(tag).map_or(tag.as_str(), String::as_str);
        let has = item.tags.iter().any(|own| own == tag);
        *by_group.entry(group).or_insert(false) |= has;
    }
    by_group.values().all(|matched| *matched)
}

fn recent(item: &Item) -> Ms {
    item.last_cooked_at.unwrap_or(Ms::MIN).max(item.created_at)
}

/// Tag name to group, from the registry.
pub fn tag_groups(tags: &[Tag]) -> BTreeMap<String, String> {
    tags.iter()
        .map(|tag| (tag.name.clone(), tag.group.clone()))
        .collect()
}

/// The ids to show, in order.
pub fn visible<'a>(
    items: impl IntoIterator<Item = &'a Item>,
    filter: &Filter,
    groups: &BTreeMap<String, String>,
    sort: Sort,
) -> Vec<String> {
    let query = tokens(&filter.query);
    let mut found: Vec<&Item> = items
        .into_iter()
        .filter(|item| in_scope(item, filter.archived))
        .filter(|item| tags_match(item, &filter.tags, groups))
        .filter(|item| matches(&query, &item.words))
        .collect();
    let by_title = |a: &&Item, b: &&Item| {
        omni_recipe_format::fold(&a.title)
            .cmp(&omni_recipe_format::fold(&b.title))
            .then_with(|| a.id.cmp(&b.id))
    };
    match sort {
        Sort::Recent => {
            found.sort_by(|a, b| recent(b).cmp(&recent(a)).then_with(|| by_title(a, b)))
        }
        Sort::Title => found.sort_by(by_title),
        Sort::MostCooked => found.sort_by(|a, b| {
            b.cook_count
                .cmp(&a.cook_count)
                .then_with(|| recent(b).cmp(&recent(a)))
                .then_with(|| by_title(a, b))
        }),
        Sort::Newest => {
            found.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| by_title(a, b)));
        }
    }
    found.into_iter().map(|item| item.id.clone()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagGroup {
    pub group: &'static str,
    pub label: &'static str,
    /// `(tag, recipes in scope carrying it)`, by name; tags nothing carries
    /// are left out unless selected.
    pub tags: Vec<(String, u32)>,
}

/// The filter sheet's sections. Counts are over the recipes in scope
/// (active, plus archived when that switch is on), so they hold offline.
pub fn filter_groups<'a>(
    items: impl IntoIterator<Item = &'a Item>,
    registry: &[Tag],
    filter: &Filter,
) -> Vec<TagGroup> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for item in items
        .into_iter()
        .filter(|item| in_scope(item, filter.archived))
    {
        for tag in &item.tags {
            *counts.entry(tag.as_str()).or_insert(0) += 1;
        }
    }
    let groups = tag_groups(registry);
    GROUPS
        .iter()
        .map(|(group, label)| {
            let mut tags: Vec<(String, u32)> = counts
                .iter()
                .map(|(tag, count)| ((*tag).to_owned(), *count))
                .chain(
                    filter
                        .tags
                        .iter()
                        .filter(|tag| !counts.contains_key(tag.as_str()))
                        .map(|tag| (tag.clone(), 0)),
                )
                .filter(|(tag, _)| groups.get(tag).map_or("other", String::as_str) == *group)
                .collect();
            tags.sort();
            TagGroup { group, label, tags }
        })
        .filter(|group| !group.tags.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: &str,
        title: &str,
        tags: &[&str],
        cooked: u32,
        last: Option<Ms>,
        created: Ms,
    ) -> Item {
        let tags: Vec<String> = tags.iter().map(|tag| (*tag).to_owned()).collect();
        Item {
            id: id.into(),
            title: title.into(),
            state: RecipeState::Active,
            words: search_words(None, title, &tags, ""),
            tags,
            cook_count: cooked,
            last_cooked_at: last,
            created_at: created,
        }
    }

    fn registry() -> Vec<Tag> {
        [
            ("main-course", "course"),
            ("dessert", "course"),
            ("italian", "cuisine"),
            ("mexican", "cuisine"),
            ("chicken", "main"),
        ]
        .into_iter()
        .map(|(name, group)| Tag {
            name: name.into(),
            group: group.into(),
            description: None,
            count: 0,
        })
        .collect()
    }

    fn items() -> Vec<Item> {
        vec![
            entry(
                "a",
                "Chicken Piccata",
                &["main-course", "italian", "chicken"],
                3,
                Some(900),
                100,
            ),
            entry("b", "Chocolate Chip Cookies", &["dessert"], 0, None, 500),
            entry(
                "c",
                "Enchiladas",
                &["main-course", "mexican", "chicken"],
                1,
                Some(300),
                200,
            ),
            entry(
                "d",
                "Ragù alla Bolognese",
                &["main-course", "italian"],
                3,
                Some(400),
                50,
            ),
        ]
    }

    fn show(filter: &Filter, sort: Sort) -> Vec<String> {
        visible(&items(), filter, &tag_groups(&registry()), sort)
    }

    fn filter(query: &str, tags: &[&str]) -> Filter {
        Filter {
            query: query.into(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            archived: false,
        }
    }

    #[test]
    fn sorts() {
        let all = Filter::default();
        assert_eq!(show(&all, Sort::Recent), ["a", "b", "d", "c"]);
        assert_eq!(show(&all, Sort::Title), ["a", "b", "c", "d"]);
        assert_eq!(show(&all, Sort::MostCooked), ["a", "d", "c", "b"]);
        assert_eq!(show(&all, Sort::Newest), ["b", "c", "a", "d"]);
    }

    #[test]
    fn search_is_prefix_matched_and_ignores_accents() {
        assert_eq!(show(&filter("ragu", &[]), Sort::Title), ["d"]);
        assert_eq!(show(&filter("CHOC chi", &[]), Sort::Title), ["b"]);
        assert_eq!(show(&filter("ital", &[]), Sort::Title), ["a", "d"]);
        assert_eq!(
            show(&filter("icken", &[]), Sort::Title),
            Vec::<String>::new()
        );
    }

    #[test]
    fn search_reads_the_document_tags_and_notes() {
        let doc: RecipeDoc = serde_json::from_str(include_str!(
            "../../../omni-recipe-format/tests/golden/lasagne-bolognese.json"
        ))
        .unwrap();
        let words = search_words(Some(&doc), &doc.title, &["make-ahead".into()], "Mum's tin");
        for query in [
            "bechamel",
            "parmigiano",
            "lasagn",
            "make ahead",
            "mums",
            "passata",
        ] {
            assert!(matches(&tokens(query), &words), "{query}");
        }
        assert!(!matches(&tokens("anchovy"), &words));
    }

    #[test]
    fn tags_are_any_within_a_group_and_all_across_groups() {
        assert_eq!(
            show(&filter("", &["italian", "mexican"]), Sort::Title),
            ["a", "c", "d"]
        );
        assert_eq!(
            show(&filter("", &["italian", "chicken"]), Sort::Title),
            ["a"]
        );
        assert_eq!(
            show(&filter("", &["italian", "mexican", "chicken"]), Sort::Title),
            ["a", "c"]
        );
        assert_eq!(
            show(&filter("", &["dessert", "chicken"]), Sort::Title),
            Vec::<String>::new()
        );
        assert_eq!(show(&filter("ench", &["chicken"]), Sort::Title), ["c"]);
    }

    #[test]
    fn archived_recipes_need_the_switch_and_trashed_never_show() {
        let mut all = items();
        all[1].state = RecipeState::Archived;
        all[2].state = RecipeState::Trashed;
        let groups = tag_groups(&registry());
        let mut filter = Filter::default();
        assert_eq!(visible(&all, &filter, &groups, Sort::Title), ["a", "d"]);
        filter.archived = true;
        assert_eq!(
            visible(&all, &filter, &groups, Sort::Title),
            ["a", "b", "d"]
        );
    }

    #[test]
    fn the_filter_sheet_lists_groups_with_counts_in_scope() {
        let mut all = items();
        all[1].state = RecipeState::Archived;
        all[0].tags.push("picnic".into());
        let groups = filter_groups(&all, &registry(), &filter("", &["dessert"]));
        let flat: Vec<(&str, Vec<(&str, u32)>)> = groups
            .iter()
            .map(|g| {
                (
                    g.group,
                    g.tags.iter().map(|(t, n)| (t.as_str(), *n)).collect(),
                )
            })
            .collect();
        assert_eq!(
            flat,
            [
                ("course", vec![("dessert", 0), ("main-course", 3)]),
                ("cuisine", vec![("italian", 2), ("mexican", 1)]),
                ("main", vec![("chicken", 2)]),
                ("other", vec![("picnic", 1)]),
            ]
        );
    }
}
