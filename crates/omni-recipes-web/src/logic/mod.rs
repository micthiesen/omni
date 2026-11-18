//! Decisions that need no browser. Everything here compiles natively and is
//! unit-tested with `cargo test -p omni-recipes-web`.

pub mod library;
pub mod outbox;
pub mod session;

use omni_api::common::Ms;
use omni_api::fond::{RecipeRecord, Tag};

/// The four screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Library,
    Recipe(String),
    Cook(String),
    Inbox,
    NotFound,
}

impl Route {
    pub fn parse(path: &str) -> Route {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match parts.as_slice() {
            [] | ["index.html"] => Route::Library,
            ["inbox"] => Route::Inbox,
            ["r", id] => Route::Recipe((*id).to_owned()),
            ["cook", id] => Route::Cook((*id).to_owned()),
            _ => Route::NotFound,
        }
    }

    pub fn path(&self) -> String {
        match self {
            Route::Library | Route::NotFound => "/".to_owned(),
            Route::Recipe(id) => format!("/r/{id}"),
            Route::Cook(id) => format!("/cook/{id}"),
            Route::Inbox => "/inbox".to_owned(),
        }
    }
}

/// The scale presets and their labels.
pub const SCALES: [(f64, &str); 5] = [(0.5, "½"), (1.0, "1"), (1.5, "1½"), (2.0, "2"), (3.0, "3")];

const SCALE_MIN: f64 = 0.1;
const SCALE_MAX: f64 = 20.0;

/// A scale factor from what a person or the stored setting says: `2`, `1.5`,
/// `1½`, `½`, `3/2`, `x2`, `2×`. Out-of-range and unreadable values are none.
pub fn parse_scale(text: &str) -> Option<f64> {
    let text = text
        .trim()
        .trim_start_matches(['x', 'X', '×'])
        .trim_end_matches(['x', 'X', '×'])
        .trim();
    const VULGAR: [(char, f64); 5] = [
        ('½', 0.5),
        ('¼', 0.25),
        ('¾', 0.75),
        ('⅓', 1.0 / 3.0),
        ('⅔', 2.0 / 3.0),
    ];
    let value = if let Some((mark, part)) = VULGAR.iter().find(|(mark, _)| text.ends_with(*mark)) {
        let whole = text.trim_end_matches(*mark).trim();
        let whole: f64 = if whole.is_empty() {
            0.0
        } else {
            f64::from(whole.parse::<u32>().ok()?)
        };
        whole + part
    } else {
        omni_recipe_format::format::parse_fraction(text)?
    };
    (value.is_finite() && (SCALE_MIN..=SCALE_MAX).contains(&value)).then_some(value)
}

/// The factor after stepping the yield by `delta` whole units ("servings
/// stepper"): 4 servings at ×1.5 is 6, one step up is 7, so ×1.75. Never
/// below one unit.
pub fn step_servings(yield_qty: f64, factor: f64, delta: i32) -> f64 {
    if !(yield_qty.is_finite() && yield_qty > 0.0) {
        return factor;
    }
    let now = yield_qty * factor;
    let stepped = if delta >= 0 {
        (now + 1e-9).floor() + f64::from(delta)
    } else {
        (now - 1e-9).ceil() + f64::from(delta)
    };
    let stepped = stepped.max(1.0f64.min(yield_qty));
    (stepped / yield_qty).clamp(SCALE_MIN, SCALE_MAX)
}

/// Whether two factors are the same setting.
pub fn same_scale(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// The colours of a typographic tile: `(background, text)`.
pub type TileColor = (&'static str, &'static str);

const TILES: [TileColor; 8] = [
    ("#b4532a", "#fff4e6"),
    ("#8a6a3b", "#fff6e0"),
    ("#5f7355", "#f3f6e8"),
    ("#3f6b6b", "#e9f5f1"),
    ("#7b4b5e", "#fbeef0"),
    ("#a3762a", "#fff7df"),
    ("#55607d", "#eef0fa"),
    ("#8c4a3c", "#fdeee6"),
];

/// A recipe without a photo is drawn on a colour taken from its course tag
/// (its first tag when it has no course, its title when it has no tags), so
/// one course shares one colour across the grid.
pub fn tile_color(record_tags: &[String], registry: &[Tag], title: &str) -> TileColor {
    let course = record_tags.iter().find(|tag| {
        registry
            .iter()
            .any(|known| known.name == **tag && known.group == "course")
    });
    let seed = course
        .or_else(|| record_tags.first())
        .map_or(title, String::as_str);
    // FNV-1a: stable across builds, unlike the std hasher.
    let hash = seed.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    TILES[hash as usize % TILES.len()]
}

/// A countdown: `1:12:40`, `4:05`, `0:00` once over.
pub fn countdown(remaining_ms: Ms) -> String {
    let seconds = (remaining_ms.max(0) + 999) / 1000;
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The cover of a recipe: the chosen image, else the first.
pub fn cover(record: &RecipeRecord) -> Option<&omni_api::fond::ImageInfo> {
    record
        .cover
        .as_ref()
        .and_then(|id| record.images.iter().find(|image| image.id == *id))
        .or_else(|| record.images.first())
}

/// The gallery order: the cover first, then the rest as stored.
pub fn gallery(record: &RecipeRecord) -> Vec<&omni_api::fond::ImageInfo> {
    let first = cover(record);
    first
        .into_iter()
        .chain(
            record
                .images
                .iter()
                .filter(|image| first.is_none_or(|cover| cover.id != image.id)),
        )
        .collect()
}

/// The whole-word columns of a chart step label set vertically in a cell
/// spanning `rows` rows: one column when the cell is tall enough, more when
/// not, and `None` when it would take more than three (the label then reads
/// horizontally). A column never splits a word, so no text can be cut; a word
/// longer than the cell makes the cell taller.
pub fn label_columns(label: &str, rows: usize) -> Option<Vec<String>> {
    // A row is at least one line of 14px text with its padding; a glyph of the
    // 600-weight label is about 8px along the column.
    const ROW_PX: usize = 32;
    const CELL_PADDING_PX: usize = 16;
    const GLYPH_PX: usize = 8;
    const MAX_COLUMNS: usize = 3;
    if rows < 3 {
        return None;
    }
    let room = (rows * ROW_PX).saturating_sub(CELL_PADDING_PX) / GLYPH_PX;
    let mut columns: Vec<String> = Vec::new();
    for word in label.split_whitespace() {
        match columns.last_mut() {
            Some(column) if column.chars().count() + 1 + word.chars().count() <= room => {
                column.push(' ');
                column.push_str(word);
            }
            _ => columns.push(word.to_owned()),
        }
    }
    (!columns.is_empty() && columns.len() <= MAX_COLUMNS).then_some(columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_round_trip() {
        assert_eq!(Route::parse("/"), Route::Library);
        assert_eq!(Route::parse(""), Route::Library);
        assert_eq!(Route::parse("/inbox"), Route::Inbox);
        assert_eq!(Route::parse("/r/rcp_1"), Route::Recipe("rcp_1".into()));
        assert_eq!(
            Route::parse("/r/rcp_1/?x=1#top"),
            Route::Recipe("rcp_1".into())
        );
        assert_eq!(
            Route::parse("/cook/cook_abcdefgh"),
            Route::Cook("cook_abcdefgh".into())
        );
        assert_eq!(Route::parse("/r"), Route::NotFound);
        assert_eq!(Route::parse("/api/sync"), Route::NotFound);
        for route in [
            Route::Library,
            Route::Inbox,
            Route::Recipe("a".into()),
            Route::Cook("b".into()),
        ] {
            assert_eq!(Route::parse(&route.path()), route);
        }
    }

    #[test]
    fn scales_parse_from_labels_and_numbers() {
        for (factor, label) in SCALES {
            assert_eq!(parse_scale(label), Some(factor), "{label}");
        }
        assert_eq!(parse_scale("1.5"), Some(1.5));
        assert_eq!(parse_scale("3/2"), Some(1.5));
        assert_eq!(parse_scale(" x2 "), Some(2.0));
        assert_eq!(parse_scale("2×"), Some(2.0));
        assert_eq!(parse_scale("2¼"), Some(2.25));
        assert_eq!(parse_scale("0"), None);
        assert_eq!(parse_scale("21"), None);
        assert_eq!(parse_scale("-1"), None);
        assert_eq!(parse_scale("lots"), None);
        assert_eq!(parse_scale(""), None);
    }

    #[test]
    fn the_servings_stepper_moves_by_whole_servings() {
        assert!(same_scale(step_servings(4.0, 1.0, 1), 1.25));
        assert!(same_scale(step_servings(4.0, 1.0, -1), 0.75));
        // From 6 servings (×1.5) up to 7, and from 2½ (×0.625) to the whole numbers around it.
        assert!(same_scale(step_servings(4.0, 1.5, 1), 1.75));
        assert!(same_scale(step_servings(4.0, 0.625, 1), 0.75));
        assert!(same_scale(step_servings(4.0, 0.625, -1), 0.5));
        // Never below one serving.
        assert!(same_scale(step_servings(4.0, 0.25, -1), 0.25));
        assert!(same_scale(step_servings(24.0, 1.0, 1), 25.0 / 24.0));
        assert!(same_scale(step_servings(0.0, 1.0, 1), 1.0));
    }

    #[test]
    fn tiles_take_their_colour_from_the_course() {
        let registry = vec![
            Tag {
                name: "dessert".into(),
                group: "course".into(),
                description: None,
                count: 1,
            },
            Tag {
                name: "italian".into(),
                group: "cuisine".into(),
                description: None,
                count: 1,
            },
        ];
        let tags = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let dessert = tile_color(&tags(&["italian", "dessert"]), &registry, "Tiramisu");
        assert_eq!(
            dessert,
            tile_color(&tags(&["dessert"]), &registry, "Cookies")
        );
        assert_eq!(
            tile_color(&tags(&["italian"]), &registry, "A"),
            tile_color(&tags(&["italian"]), &registry, "B")
        );
        assert_eq!(
            tile_color(&[], &registry, "Soup"),
            tile_color(&[], &registry, "Soup")
        );
        assert!(TILES.contains(&dessert));
    }

    #[test]
    fn countdowns_round_up_to_the_second() {
        assert_eq!(countdown(4_360_000), "1:12:40");
        assert_eq!(countdown(245_000), "4:05");
        assert_eq!(countdown(900), "0:01");
        assert_eq!(countdown(0), "0:00");
        assert_eq!(countdown(-5), "0:00");
    }

    #[test]
    fn a_vertical_label_keeps_every_word() {
        // Tall enough: one column.
        assert_eq!(
            label_columns("season, dredge 5 min", 8),
            Some(vec!["season, dredge 5 min".to_owned()])
        );
        // Three rows: whole words, in order, nothing dropped.
        let columns = label_columns("season, dredge 5 min", 3).unwrap();
        assert_eq!(columns, ["season,", "dredge 5", "min"]);
        assert_eq!(columns.join(" "), "season, dredge 5 min");
        // A long word is never split; it gets a column of its own.
        assert_eq!(
            label_columns("caramelise 40 min", 3),
            Some(vec!["caramelise".to_owned(), "40 min".to_owned()])
        );
        // Too short a cell, or too many columns, reads horizontally.
        assert_eq!(label_columns("rest 5 min", 2), None);
        assert_eq!(
            label_columns("whisk, fold, pour, scrape and level 2 min", 3),
            None
        );
    }
}
