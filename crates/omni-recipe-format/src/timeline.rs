//! A schedule for one cook working through the steps in written order:
//! active steps occupy the cook, passive steps run in the background once
//! started, and setup steps (no inputs) start just in time.

use serde::{Deserialize, Serialize};

use crate::index::Index;
use crate::model::{Attention, Id, RecipeDoc};
use crate::text::chart_label;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimelineRow {
    /// Minutes from the start of cooking.
    pub start: u64,
    pub end: u64,
    /// The id of the step's component.
    pub component: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_name: Option<String>,
    /// The step id.
    pub step: Id,
    /// A step without a time counts as active and takes no time.
    pub attention: Attention,
    /// The step's chart label.
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DerivedTime {
    pub active_min: u64,
    pub total_min: u64,
}

/// One row per step, in written order.
pub fn timeline(doc: &RecipeDoc) -> Vec<TimelineRow> {
    let ix = Index::build(doc);
    let mut ready: Vec<(&str, u64)> = Vec::new();
    let ready_at = |ready: &[(&str, u64)], id: &str| {
        ready
            .iter()
            .rev()
            .find(|(step, _)| *step == id)
            .map_or(0, |(_, at)| *at)
    };
    let mut cook_free = 0u64;
    let mut rows: Vec<TimelineRow> = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    for (component, step) in ix.steps() {
        let duration = step.time.map_or(0, |time| u64::from(time.longest()));
        let attention = step.time.map_or(Attention::Active, |time| time.attention);
        let mut row = TimelineRow {
            start: 0,
            end: duration,
            component: component.id.clone(),
            component_name: component.name.clone().filter(|name| !name.is_empty()),
            step: step.id.clone(),
            attention,
            label: chart_label(step),
        };
        if step.inputs.is_empty() {
            pending.push(rows.len());
            rows.push(row);
            continue;
        }
        let sources: Vec<&str> = step
            .inputs
            .iter()
            .filter_map(|input| ix.source_step(&input.r#ref))
            .collect();
        let inputs_ready = sources
            .iter()
            .map(|source| ready_at(&ready, source))
            .max()
            .unwrap_or(0);
        let mut start = cook_free.max(inputs_ready);
        pending.retain(|at| {
            let setup = &mut rows[*at];
            if !sources.contains(&setup.step.as_str()) {
                return true;
            }
            let duration = setup.end;
            setup.start = start.saturating_sub(duration);
            setup.end = setup.start.saturating_add(duration);
            start = start.max(setup.end);
            false
        });
        let end = start.saturating_add(duration);
        ready.push((&step.id, end));
        cook_free = if attention == Attention::Passive {
            start
        } else {
            end
        };
        row.start = start;
        row.end = end;
        rows.push(row);
    }
    rows
}

/// Hands-on and start-to-finish minutes, rounded up to five, from the step
/// times and the graph.
pub fn derived_time(doc: &RecipeDoc) -> DerivedTime {
    let rows = timeline(doc);
    let active = rows
        .iter()
        .filter(|row| row.attention != Attention::Passive)
        .fold(0u64, |sum, row| sum.saturating_add(row.end - row.start));
    let total = rows.iter().map(|row| row.end).max().unwrap_or(0);
    let up5 = |minutes: u64| minutes.div_ceil(5).saturating_mul(5);
    DerivedTime {
        active_min: up5(active),
        total_min: up5(total),
    }
}

fn clock(minutes: u64) -> String {
    format!("{}:{:02}", minutes / 60, minutes % 60)
}

/// The schedule as text, one line per step in starting order.
pub fn render_timeline(doc: &RecipeDoc) -> String {
    let mut rows: Vec<(usize, TimelineRow)> = timeline(doc).into_iter().enumerate().collect();
    let total = rows.iter().map(|(_, row)| row.end).max().unwrap_or(0);
    rows.sort_by_key(|(written, row)| (row.start, *written));
    let mut out: Vec<String> = rows
        .iter()
        .map(|(_, row)| {
            let kind = match row.attention {
                Attention::Active => "hands-on  ",
                Attention::Passive => "unattended",
            };
            let name = match &row.component_name {
                Some(name) => format!("{name}: "),
                None => String::new(),
            };
            format!(
                "{} → {}  {kind}  {name}{}",
                clock(row.start),
                clock(row.end),
                row.label
            )
        })
        .collect();
    out.push(format!("elapsed {}", clock(total)));
    out.join("\n")
}
