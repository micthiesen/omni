//! What cook mode derives from the graph: which steps are ready, what can be
//! done while a passive step runs, each step's inputs, and the equipment.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::index::{Index, Kind};
use crate::list::{IngredientGroup, ingredient_groups};
use crate::model::{Id, RecipeDoc, Step};
use crate::text::use_label;

/// Whether every step that produces one of `step`'s inputs is done.
pub(crate) fn is_ready(ix: &Index<'_>, step: &Step, done: &HashSet<&str>) -> bool {
    step.inputs.iter().all(|input| {
        ix.source_step(&input.r#ref)
            .is_none_or(|source| done.contains(source))
    })
}

fn id_set<S: AsRef<str>>(ids: &[S]) -> HashSet<&str> {
    ids.iter().map(AsRef::as_ref).collect()
}

/// The steps that can start now: not done, and nothing they wait for is
/// outstanding. In written order.
pub fn ready_steps<S: AsRef<str>>(doc: &RecipeDoc, done: &[S]) -> Vec<Id> {
    let done = id_set(done);
    let ix = Index::build(doc);
    ix.steps()
        .filter(|(_, step)| !done.contains(step.id.as_str()) && is_ready(&ix, step, &done))
        .map(|(_, step)| step.id.clone())
        .collect()
}

/// The steps that directly produce `step`'s inputs, without repeats.
pub fn step_sources(ix: &Index<'_>, step: &Step) -> Vec<Id> {
    let mut sources: Vec<Id> = Vec::new();
    for input in &step.inputs {
        if let Some(source) = ix.source_step(&input.r#ref)
            && !sources.iter().any(|known| known == source)
        {
            sources.push(source.to_owned());
        }
    }
    sources
}

/// Whether `step` needs `other` to be done first, directly or through other
/// steps.
pub fn depends_on(ix: &Index<'_>, step: &str, other: &str) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: Vec<&str> = vec![step];
    while let Some(id) = queue.pop() {
        let Some(current) = ix.step(id) else { continue };
        for input in &current.inputs {
            let Some(source) = ix.source_step(&input.r#ref) else {
                continue;
            };
            if source == other {
                return true;
            }
            if seen.insert(source) {
                queue.push(source);
            }
        }
    }
    false
}

/// What the cook can do while the passive step `running` is under way: the
/// ready steps that do not depend on it. In written order.
pub fn meanwhile<S: AsRef<str>>(doc: &RecipeDoc, running: &str, done: &[S]) -> Vec<Id> {
    let done: HashSet<&str> = done
        .iter()
        .map(AsRef::as_ref)
        .filter(|id| *id != running)
        .collect();
    let ix = Index::build(doc);
    ix.steps()
        .map(|(_, step)| step)
        .filter(|step| step.id != running && !done.contains(step.id.as_str()))
        .filter(|step| is_ready(&ix, step, &done) && !depends_on(&ix, &step.id, running))
        .map(|step| step.id.clone())
        .collect()
}

/// One input of a step, as cook mode lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputLabel {
    /// `step/ref` or `step/ref:part`: the key of the same use in the chart.
    pub key: String,
    pub r#ref: Id,
    /// What the ref names; absent when it resolves to nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Kind>,
    /// "2 tbsp unsalted butter, cold (the rest)", "seared cutlets".
    pub label: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

/// The inputs of one step with the amounts for that use at `factor`, in
/// input order. `step` must be borrowed from the indexed document.
pub fn step_inputs(ix: &Index<'_>, step: &Step, factor: f64) -> Vec<InputLabel> {
    step.inputs
        .iter()
        .map(|input| InputLabel {
            key: input.key(&step.id),
            r#ref: input.r#ref.clone(),
            target: ix.kind(&input.r#ref),
            label: use_label(ix, input, factor, false),
            optional: ix
                .ingredient(&input.r#ref)
                .is_some_and(|ingredient| ingredient.optional),
        })
        .collect()
}

/// What to get out and prep before cooking: every ingredient with its total
/// at `factor`, grouped by component.
pub fn gather_list(doc: &RecipeDoc, factor: f64) -> Vec<IngredientGroup> {
    ingredient_groups(doc, factor)
}

/// Every piece of equipment the steps name, once each, in order of first use.
pub fn equipment(doc: &RecipeDoc) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for component in &doc.components {
        for item in component.steps.iter().flat_map(|step| &step.equipment) {
            let item = item.trim();
            if !item.is_empty() && !all.iter().any(|known| known == item) {
                all.push(item.to_owned());
            }
        }
    }
    all
}
