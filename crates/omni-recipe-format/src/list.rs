//! The list view: header, ingredients grouped by component, numbered steps.
//! [`list_view`] is the structured form; [`render_list`] is its Markdown.

use serde::{Deserialize, Serialize};

use crate::format::{fmt_amount, fmt_factor, fmt_time, inflect};
use crate::index::{Index, Kind};
use crate::model::{Component, Id, Ingredient, RecipeDoc};
use crate::text::{Segment, render_step_segments};
use crate::timeline::derived_time;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListView {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// "Makes 6 servings".
    pub makes: String,
    /// The scale as shown, "1.5", when it is not 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaled: Option<String>,
    /// Hands-on minutes: the document's override, or derived from the steps.
    pub active_min: u64,
    /// Start-to-finish minutes.
    pub total_min: u64,
    /// "25 min hands-on, 25 min start to finish".
    pub time: String,
    /// One group per component, in document order.
    pub groups: Vec<IngredientGroup>,
    /// Every step, numbered across components.
    pub steps: Vec<ListStep>,
}

impl ListView {
    /// The header line under the title: yield, scale and time.
    pub fn meta(&self) -> String {
        match &self.scaled {
            Some(scale) => format!("{} (scaled ×{scale}) · {}", self.makes, self.time),
            None => format!("{} · {}", self.makes, self.time),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngredientGroup {
    /// The component id.
    pub component: Id,
    /// The group heading; present when the recipe has several components.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    /// "make ahead: keeps 3 days in the fridge".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make_ahead: Option<String>,
    pub lines: Vec<IngredientLine>,
    /// Names of earlier components this one uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from_above: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IngredientLine {
    /// The ingredient id.
    pub id: Id,
    /// "4 tbsp unsalted butter, cold, divided".
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListStep {
    /// The step id.
    pub id: Id,
    /// 1-based, counted across components.
    pub number: usize,
    /// The id of the step's component.
    pub component: Id,
    /// The rendered text.
    pub text: String,
    /// The same text split at its placeholders.
    pub segments: Vec<Segment>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

fn component_name(component: &Component) -> &str {
    component.name.as_deref().unwrap_or(&component.id)
}

/// "make ahead: keeps 3 days in the fridge".
pub(crate) fn make_ahead_note(component: &Component) -> Option<String> {
    component
        .make_ahead
        .as_ref()
        .map(|ahead| format!("make ahead: keeps {} in the {}", ahead.keeps, ahead.r#where))
}

/// One ingredient as the list shows it: total, the `alt` amount, name, prep,
/// then the markers `divided`, `to taste`, the note and `(optional)`.
pub fn ingredient_line(ix: &Index<'_>, ingredient: &Ingredient, factor: f64) -> String {
    let factor = if ingredient.scales { factor } else { 1.0 };
    let unit = ingredient.unit.as_deref();
    let qty = ingredient.qty.map(|qty| qty * factor);
    let mut line = format!("{} ", fmt_amount(qty, unit));
    if let Some(alt) = &ingredient.alt {
        let amount = fmt_amount(Some(alt.qty * factor), Some(&alt.unit));
        line.push_str(&format!("({amount}) "));
    }
    line.push_str(&inflect(&ingredient.name, qty, unit, ingredient));
    let mut line = line.trim().to_owned();
    if let Some(prep) = ingredient.prep.as_deref().filter(|prep| !prep.is_empty()) {
        line.push_str(&format!(", {prep}"));
    }
    let uses = ix.consumers(&ingredient.id);
    let parts_only = uses.iter().all(|u| u.input.part.is_some());
    if uses.len() > 1 && ingredient.qty.is_some() && !parts_only {
        line.push_str(", divided");
    }
    if ingredient.to_taste {
        line.push_str(", to taste");
    }
    if let Some(note) = ingredient.note.as_deref().filter(|note| !note.is_empty()) {
        line.push_str(&format!(" ({note})"));
    }
    if ingredient.optional {
        line.push_str(" (optional)");
    }
    line
}

/// The ingredient groups of the list view, which are also cook mode's gather
/// list: totals and prep, grouped by component.
pub fn ingredient_groups(doc: &RecipeDoc, factor: f64) -> Vec<IngredientGroup> {
    let ix = Index::build(doc);
    let multi = doc.components.len() > 1;
    doc.components
        .iter()
        .map(|component| {
            let mut from_above: Vec<String> = Vec::new();
            let mut seen: Vec<&str> = Vec::new();
            for input in component.steps.iter().flat_map(|step| &step.inputs) {
                let id = input.r#ref.as_str();
                if ix.kind(id) == Some(Kind::Component) && !seen.contains(&id) {
                    seen.push(id);
                    from_above.extend(ix.component_of(id).map(|c| component_name(c).to_owned()));
                }
            }
            IngredientGroup {
                component: component.id.clone(),
                heading: multi.then(|| component_name(component).to_owned()),
                make_ahead: make_ahead_note(component),
                lines: component
                    .ingredients
                    .iter()
                    .map(|ingredient| IngredientLine {
                        id: ingredient.id.clone(),
                        text: ingredient_line(&ix, ingredient, factor),
                        optional: ingredient.optional,
                    })
                    .collect(),
                from_above,
            }
        })
        .collect()
}

/// The list view at `factor`, as data.
pub fn list_view(doc: &RecipeDoc, factor: f64) -> ListView {
    let ix = Index::build(doc);
    let (active_min, total_min) = match doc.time {
        Some(time) => (u64::from(time.active_min), u64::from(time.total_min)),
        None => {
            let derived = derived_time(doc);
            (derived.active_min, derived.total_min)
        }
    };
    let clock = |minutes: u64| fmt_time(u32::try_from(minutes).unwrap_or(u32::MAX), None);
    let made = fmt_amount(Some(doc.r#yield.qty * factor), None);
    let mut steps = Vec::new();
    for (component, step) in ix.steps() {
        let segments = render_step_segments(&ix, step, factor);
        steps.push(ListStep {
            id: step.id.clone(),
            number: steps.len() + 1,
            component: component.id.clone(),
            text: segments.iter().map(Segment::text).collect(),
            segments,
            optional: step.optional,
        });
    }
    ListView {
        title: doc.title.clone(),
        summary: doc.summary.clone().filter(|summary| !summary.is_empty()),
        makes: format!("Makes {made} {}", doc.r#yield.unit),
        #[allow(clippy::float_cmp)]
        scaled: (factor != 1.0).then(|| fmt_factor(factor)),
        active_min,
        total_min,
        time: format!(
            "{} hands-on, {} start to finish",
            clock(active_min),
            clock(total_min)
        ),
        groups: ingredient_groups(doc, factor),
        steps,
    }
}

/// The list view at `factor`, as Markdown.
pub fn render_list(doc: &RecipeDoc, factor: f64) -> String {
    let view = list_view(doc, factor);
    let mut out = vec![format!("# {}", view.title), String::new()];
    if let Some(summary) = &view.summary {
        out.extend([summary.clone(), String::new()]);
    }
    out.extend([
        view.meta(),
        String::new(),
        "## Ingredients".to_owned(),
        String::new(),
    ]);
    for group in &view.groups {
        if let Some(heading) = &group.heading {
            let note = match &group.make_ahead {
                Some(note) => format!(" ({note})"),
                None => String::new(),
            };
            out.extend([format!("**{heading}**{note}"), String::new()]);
        }
        out.extend(group.lines.iter().map(|line| format!("- {}", line.text)));
        out.extend(
            group
                .from_above
                .iter()
                .map(|name| format!("- {name} (from above)")),
        );
        out.push(String::new());
    }
    out.extend(["## Steps".to_owned(), String::new()]);
    let mut steps = view.steps.iter().peekable();
    for (group, component) in view.groups.iter().zip(&doc.components) {
        if let Some(heading) = &group.heading {
            out.extend([format!("**{heading}**"), String::new()]);
        }
        for step in steps.by_ref().take(component.steps.len()) {
            let tag = if step.optional { " *(optional)*" } else { "" };
            out.push(format!("{}. {}{tag}", step.number, step.text));
        }
        out.push(String::new());
    }
    format!("{}\n", out.join("\n").trim_end())
}
