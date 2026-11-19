//! Decisions that need no browser. Everything here compiles natively and is
//! unit-tested with `cargo test -p omni-recipes-web`.

pub mod library;
pub mod outbox;
pub mod session;

use omni_api::common::Ms;
use omni_api::fond::{RecipeRecord, Tag};

/// The five screens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Library,
    Recipe(String),
    Cook(String),
    Inbox,
    Requests,
    NotFound,
}

impl Route {
    pub fn parse(path: &str) -> Route {
        let path = path.split(['?', '#']).next().unwrap_or_default();
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match parts.as_slice() {
            [] | ["index.html"] => Route::Library,
            ["inbox"] => Route::Inbox,
            ["requests"] => Route::Requests,
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
            Route::Requests => "/requests".to_owned(),
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

/// The factor after one press of the yield stepper. The yield moves the way
/// an amount of its unit is measured (`omni_recipe_format::measure::step`):
/// servings and other counted things by one, a yield in millilitres or grams
/// by steps that grow with it, cups by quarters. 4 servings at ×1.5 is 6, one
/// press up is 7, so ×1.75. A counted yield never goes below one.
pub fn step_servings(yield_qty: f64, unit: &str, factor: f64, up: bool) -> f64 {
    if !(yield_qty.is_finite() && yield_qty > 0.0) {
        return factor;
    }
    let now = yield_qty * factor;
    let mut stepped = omni_recipe_format::measure::step(now, Some(unit), up);
    let measured = omni_recipe_format::format::is_known_unit(unit)
        || omni_recipe_format::format::unit_alias(unit).is_some();
    if !measured {
        stepped = stepped.max(1.0f64.min(yield_qty));
    }
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

/// One chart cell as the column sizing sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartLabel<'a> {
    pub c0: usize,
    pub c1: usize,
    pub label: &'a str,
}

const STEP_MIN: f64 = 64.0;
const STEP_MAX: f64 = 120.0;
const LEAD_MIN: f64 = 130.0;
const LEAD_MAX: f64 = 260.0;
/// Cell padding and border.
const CELL_PAD: f64 = 17.0;
/// The advance of a 14px glyph, on average, for when text cannot be
/// measured: the 600-weight step labels and the regular ingredient labels.
const GLYPH_BOLD: f64 = 7.9;
const GLYPH: f64 = 7.3;

/// The width of chart text when it cannot be measured: step labels are bold.
pub fn estimated_text_width(text: &str, bold: bool) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let chars = text.chars().count() as f64;
    chars * if bold { GLYPH_BOLD } else { GLYPH }
}

/// The pixel width of every column of a chart block with `cols` columns
/// shown in `avail` pixels; column 0 is the ingredient column. All text is
/// horizontal. A step column is as wide as its label up to 120px, where the
/// label wraps, and never narrower than 64px or its longest word. The
/// ingredient column takes what is left, at least 130px. The columns add up
/// to `avail` whenever they can; when even the narrowest columns do not fit,
/// they add up to more and the chart scrolls sideways. `text_width` measures
/// a label in pixels; its second argument says whether the text is bold.
pub fn chart_widths(
    cells: &[ChartLabel<'_>],
    cols: usize,
    avail: f64,
    text_width: &dyn Fn(&str, bool) -> f64,
) -> Vec<f64> {
    if cols == 0 {
        return Vec::new();
    }
    let steps = cols - 1;
    let mut pref = vec![0.0_f64; cols];
    let mut min = vec![0.0_f64; cols];
    let wanted = |label: &str| {
        // A pixel to spare, so rounding never pushes a word onto a new line.
        let px = |text: &str| (text_width(text, true) + CELL_PAD + 1.0).ceil();
        let pref = px(label).clamp(STEP_MIN, STEP_MAX);
        let word = label.split_whitespace().map(px).fold(0.0, f64::max);
        (pref, word.clamp(STEP_MIN, pref))
    };
    // Cells in one column first, then the ones that span several.
    let mut by_span: Vec<&ChartLabel<'_>> = cells.iter().filter(|cell| cell.c0 >= 1).collect();
    by_span.sort_by_key(|cell| cell.c1 - cell.c0);
    for cell in by_span {
        let (cell_pref, cell_min) = wanted(cell.label);
        let span = cell.c0..=cell.c1.min(cols - 1);
        #[allow(clippy::cast_precision_loss)]
        let count = span.clone().count() as f64;
        for (sizes, needed) in [(&mut pref, cell_pref), (&mut min, cell_min)] {
            let short = needed - span.clone().map(|c| sizes[c]).sum::<f64>();
            if short > 0.0 {
                for c in span.clone() {
                    sizes[c] += short / count;
                }
            }
        }
    }
    let lead_pref = cells
        .iter()
        .filter(|cell| cell.c0 == 0)
        .map(|cell| (text_width(cell.label, false) + CELL_PAD + 1.0).ceil())
        .fold(0.0, f64::max)
        .clamp(LEAD_MIN, LEAD_MAX);
    let (sum_pref, sum_min) = (pref.iter().sum::<f64>(), min.iter().sum::<f64>());
    #[allow(clippy::cast_precision_loss)]
    let count = steps.max(1) as f64;
    let mut widths = if LEAD_MIN + sum_pref <= avail {
        // Room to spare: the ingredient column grows to one line of its
        // longest label, and what is still left widens the steps evenly.
        let lead = (avail - sum_pref).min(if steps == 0 { avail } else { lead_pref });
        let extra = (avail - sum_pref - lead) / count;
        pref.iter().map(|width| width + extra).collect::<Vec<_>>()
    } else if LEAD_MIN + sum_min <= avail {
        // Step labels wrap onto more lines, all by the same share.
        let share = (avail - LEAD_MIN - sum_min) / (sum_pref - sum_min);
        pref.iter()
            .zip(&min)
            .map(|(pref, min)| min + (pref - min) * share)
            .collect()
    } else {
        min
    };
    let rest: f64 = widths.iter().skip(1).sum();
    widths[0] = (avail - rest).max(LEAD_MIN);
    widths
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
    fn the_yield_stepper_moves_the_way_its_unit_is_measured() {
        let step = |qty, factor, up| step_servings(qty, "servings", factor, up);
        assert!(same_scale(step(4.0, 1.0, true), 1.25));
        assert!(same_scale(step(4.0, 1.0, false), 0.75));
        // From 6 servings (×1.5) up to 7, and from 2½ (×0.625) to the whole numbers around it.
        assert!(same_scale(step(4.0, 1.5, true), 1.75));
        assert!(same_scale(step(4.0, 0.625, true), 0.75));
        assert!(same_scale(step(4.0, 0.625, false), 0.5));
        // Never below one serving.
        assert!(same_scale(step(4.0, 0.25, false), 0.25));
        assert!(same_scale(step(24.0, 1.0, true), 25.0 / 24.0));
        assert!(same_scale(step(0.0, 1.0, true), 1.0));
        // A yield that is an amount steps like one: 250 ml by 25, 500 g by
        // 25, 2 cups by a half, half a cup by a quarter.
        assert!(same_scale(
            step_servings(250.0, "ml", 1.0, true),
            275.0 / 250.0
        ));
        assert!(same_scale(
            step_servings(250.0, "ml", 1.0, false),
            225.0 / 250.0
        ));
        assert!(same_scale(
            step_servings(500.0, "g", 1.0, true),
            525.0 / 500.0
        ));
        assert!(same_scale(step_servings(2.0, "cups", 1.0, true), 1.25));
        assert!(same_scale(step_servings(0.5, "cup", 1.0, false), 0.5));
        assert!(same_scale(step_servings(1.0, "l", 1.0, true), 1.1));
        // A scaled yield that is off the grid snaps onto it: 250 ml at ×1.5
        // is 375, and the next press is 400.
        assert!(same_scale(
            step_servings(250.0, "ml", 1.5, true),
            400.0 / 250.0
        ));
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

    fn labels<'a>(steps: &[&'a str]) -> Vec<ChartLabel<'a>> {
        let mut cells = vec![ChartLabel {
            c0: 0,
            c1: 0,
            label: "120 ml (½ cup) olive oil",
        }];
        cells.extend(steps.iter().enumerate().map(|(n, label)| ChartLabel {
            c0: n + 1,
            c1: n + 1,
            label,
        }));
        cells
    }

    #[test]
    fn a_small_chart_fills_the_width_with_horizontal_columns() {
        // The vinaigrette on a phone: three steps beside the ingredients.
        let steps = ["soak shallot 10 min", "add", "shake"];
        let widths = chart_widths(&labels(&steps), 4, 358.0, &estimated_text_width);
        assert!(
            (widths.iter().sum::<f64>() - 358.0).abs() < 0.01,
            "{widths:?}"
        );
        assert!(widths[0] >= 130.0, "{widths:?}");
        // The long label wraps in a wide column; short ones stay narrow.
        assert!(widths[1] > widths[2] && widths[1] <= 120.0, "{widths:?}");
        assert!(widths[2] >= 64.0 && widths[3] >= 64.0, "{widths:?}");
        // On a wide screen it still fills the width.
        let wide = chart_widths(&labels(&steps), 4, 696.0, &estimated_text_width);
        assert!((wide.iter().sum::<f64>() - 696.0).abs() < 0.01, "{wide:?}");
        assert!(wide[0] <= 260.0);
    }

    #[test]
    fn a_big_chart_scrolls_only_when_its_narrowest_columns_do_not_fit() {
        let seven = [
            "heat oven",
            "whisk",
            "cream 4 min",
            "beat in",
            "mix",
            "fold",
            "bake 11 min",
        ];
        let phone = chart_widths(&labels(&seven), 8, 358.0, &estimated_text_width);
        assert!(phone.iter().sum::<f64>() > 358.0, "{phone:?}");
        assert!((phone[0] - 130.0).abs() < 0.01);
        assert!(
            phone.iter().skip(1).all(|width| *width >= 64.0),
            "{phone:?}"
        );
        // The same chart fits a phone on its side.
        let landscape = chart_widths(&labels(&seven), 8, 696.0, &estimated_text_width);
        assert!(
            (landscape.iter().sum::<f64>() - 696.0).abs() < 0.01,
            "{landscape:?}"
        );
        // A step that spans columns widens them only when they are too narrow.
        let mut cells = labels(&["add", "shake"]);
        cells.push(ChartLabel {
            c0: 1,
            c1: 2,
            label: "refrigerate overnight",
        });
        let spanning = chart_widths(&cells, 3, 2000.0, &estimated_text_width);
        assert!(spanning[1] + spanning[2] >= 120.0);
    }
}
