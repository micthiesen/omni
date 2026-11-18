//! Id-addressed edit operations, scaling, and the change list between two
//! revisions.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::index::{Index, Kind, Node};
use crate::model::{
    Component, Id, Ingredient, Input, RecipeDoc, Source, Step, TimeOverride, Yield,
};
use crate::py;

/// Field names and their new values, merged into the target as JSON.
pub type Fields = Map<String, Value>;

/// One edit. Operations address ingredients, steps and components by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum Op {
    /// Merge `fields` into an ingredient, step or component; `unset` removes
    /// fields. The id itself changes only through `rename`.
    Set {
        target: Id,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        fields: Fields,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unset: Vec<String>,
    },
    /// Change the title, summary, yield, time or source.
    SetRecipe {
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        fields: Fields,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unset: Vec<String>,
    },
    /// Add, change or remove one input of a step. With several inputs for the
    /// same ref (parts), the first is the one changed or removed.
    SetInput {
        step: Id,
        r#ref: Id,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        fields: Fields,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        unset: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        remove: bool,
    },
    /// Add an ingredient to a component: after `after`, or first.
    AddIngredient {
        component: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Id>,
        ingredient: Box<Ingredient>,
    },
    /// Add a step to a component: after `after`, or first.
    AddStep {
        component: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Id>,
        step: Box<Step>,
    },
    /// Add a component: after `after`, or first.
    AddComponent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Id>,
        component: Box<Component>,
    },
    /// Remove an ingredient, step or component.
    Remove { target: Id },
    /// Move an ingredient, step or component after another of its kind, or
    /// first in its list.
    Move {
        target: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Id>,
    },
    /// Replace part of a step's text. `find` must occur exactly once.
    ReplaceText {
        target: Id,
        find: String,
        replace: String,
    },
    /// Rename an id everywhere: its definition, every input and placeholder.
    Rename { target: Id, to: Id },
    /// Multiply the yield and every amount that scales.
    Rescale { factor: f64 },
}

impl Op {
    /// The wire name of the operation.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set",
            Self::SetRecipe { .. } => "setRecipe",
            Self::SetInput { .. } => "setInput",
            Self::AddIngredient { .. } => "addIngredient",
            Self::AddStep { .. } => "addStep",
            Self::AddComponent { .. } => "addComponent",
            Self::Remove { .. } => "remove",
            Self::Move { .. } => "move",
            Self::ReplaceText { .. } => "replaceText",
            Self::Rename { .. } => "rename",
            Self::Rescale { .. } => "rescale",
        }
    }
}

/// An operation that could not be applied. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("ops[{index}] ({op}): {message}")]
pub struct OpError {
    /// The position of the failing operation in the list.
    pub index: usize,
    pub op: &'static str,
    pub message: String,
}

/// Applies `ops` in order to a copy of `doc`: all or nothing. The result is
/// well-formed but not validated; run [`crate::validate`] on it.
pub fn apply_ops(doc: &RecipeDoc, ops: &[Op]) -> Result<RecipeDoc, OpError> {
    let mut new = doc.clone();
    for (index, op) in ops.iter().enumerate() {
        apply(&mut new, op).map_err(|message| OpError {
            index,
            op: op.name(),
            message,
        })?;
    }
    Ok(new)
}

/// `doc` with the yield and every scaling amount multiplied by `factor`:
/// ingredient totals, `alt` amounts and explicit use amounts. Ingredients
/// with `scales: false` and their uses keep their amounts.
pub fn scaled(doc: &RecipeDoc, factor: f64) -> RecipeDoc {
    let fixed: Vec<Id> = {
        let ix = Index::build(doc);
        ix.ids()
            .iter()
            .filter(|id| ix.ingredient(id).is_some_and(|i| !i.scales))
            .map(|id| (*id).to_owned())
            .collect()
    };
    let mut new = doc.clone();
    new.r#yield.qty *= factor;
    for component in &mut new.components {
        for ingredient in component.ingredients.iter_mut().filter(|i| i.scales) {
            if let Some(qty) = &mut ingredient.qty {
                *qty *= factor;
            }
            if let Some(alt) = &mut ingredient.alt {
                alt.qty *= factor;
            }
        }
        for input in component.steps.iter_mut().flat_map(|s| &mut s.inputs) {
            if let Some(qty) = &mut input.qty
                && !fixed.contains(&input.r#ref)
            {
                *qty *= factor;
            }
        }
    }
    new
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Component(usize),
    Ingredient(usize, usize),
    Step(usize, usize),
}

impl Slot {
    fn kind(self) -> Kind {
        match self {
            Self::Component(_) => Kind::Component,
            Self::Ingredient(..) => Kind::Ingredient,
            Self::Step(..) => Kind::Step,
        }
    }
}

fn not_found(id: &str) -> String {
    format!("no ingredient, step or component has id \"{id}\"")
}

fn locate(doc: &RecipeDoc, target: &str) -> Result<Slot, String> {
    for (c, component) in doc.components.iter().enumerate() {
        if component.id == target {
            return Ok(Slot::Component(c));
        }
        if let Some(i) = component.ingredients.iter().position(|i| i.id == target) {
            return Ok(Slot::Ingredient(c, i));
        }
        if let Some(s) = component.steps.iter().position(|s| s.id == target) {
            return Ok(Slot::Step(c, s));
        }
    }
    Err(not_found(target))
}

fn locate_component(doc: &RecipeDoc, target: &str) -> Result<usize, String> {
    match locate(doc, target)? {
        Slot::Component(c) => Ok(c),
        _ => Err(format!("\"{target}\" is not a component")),
    }
}

fn locate_step<'a>(doc: &'a mut RecipeDoc, target: &str) -> Result<&'a mut Step, String> {
    match locate(doc, target)? {
        Slot::Step(c, s) => Ok(&mut doc.components[c].steps[s]),
        _ => Err(format!("\"{target}\" is not a step")),
    }
}

/// `node` with `fields` merged in and `unset` removed, read back as `T` so a
/// field of the wrong name or shape is refused here.
fn merged<T: Serialize + DeserializeOwned>(
    node: &T,
    fields: &Fields,
    unset: &[String],
) -> Result<T, String> {
    let Ok(Value::Object(mut map)) = serde_json::to_value(node) else {
        return Err("the target could not be read as an object".to_owned());
    };
    for (key, value) in fields {
        map.insert(key.clone(), value.clone());
    }
    for key in unset {
        map.shift_remove(key);
    }
    serde_json::from_value(Value::Object(map)).map_err(|error| error.to_string())
}

fn keep_id(old: &str, new: &str) -> Result<(), String> {
    if old == new {
        Ok(())
    } else {
        Err(format!(
            "set cannot change an id; use {{\"op\": \"rename\", \"target\": \"{old}\", \"to\": \"{new}\"}}"
        ))
    }
}

/// The part of the document `setRecipe` may change.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    r#yield: Yield,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    time: Option<TimeOverride>,
    source: Source,
}

fn apply(doc: &mut RecipeDoc, op: &Op) -> Result<(), String> {
    match op {
        Op::Set {
            target,
            fields,
            unset,
        } => match locate(doc, target)? {
            Slot::Component(c) => {
                let new: Component = merged(&doc.components[c], fields, unset)?;
                keep_id(target, &new.id)?;
                doc.components[c] = new;
            }
            Slot::Ingredient(c, i) => {
                let new: Ingredient = merged(&doc.components[c].ingredients[i], fields, unset)?;
                keep_id(target, &new.id)?;
                doc.components[c].ingredients[i] = new;
            }
            Slot::Step(c, s) => {
                let new: Step = merged(&doc.components[c].steps[s], fields, unset)?;
                keep_id(target, &new.id)?;
                doc.components[c].steps[s] = new;
            }
        },
        Op::SetRecipe { fields, unset } => {
            let header = Header {
                title: doc.title.clone(),
                summary: doc.summary.clone(),
                r#yield: doc.r#yield.clone(),
                time: doc.time,
                source: doc.source.clone(),
            };
            let new = merged(&header, fields, unset)?;
            doc.title = new.title;
            doc.summary = new.summary;
            doc.r#yield = new.r#yield;
            doc.time = new.time;
            doc.source = new.source;
        }
        Op::SetInput {
            step,
            r#ref,
            fields,
            unset,
            remove,
        } => {
            let target = locate_step(doc, step)?;
            let found = target.inputs.iter().position(|input| input.r#ref == *r#ref);
            match (found, remove) {
                (None, true) => return Err(format!("step \"{step}\" has no input \"{ref}\"")),
                (Some(at), true) => {
                    target.inputs.remove(at);
                }
                (Some(at), false) => target.inputs[at] = merged(&target.inputs[at], fields, unset)?,
                (None, false) => {
                    let input = merged(&Input::all(r#ref.clone()), fields, &[])?;
                    target.inputs.push(input);
                }
            }
        }
        Op::AddIngredient {
            component,
            after,
            ingredient,
        } => {
            let c = locate_component(doc, component)?;
            let at = match after {
                None => 0,
                Some(after) => match locate(doc, after)? {
                    Slot::Ingredient(owner, i) if owner == c => i + 1,
                    _ => {
                        return Err(format!(
                            "\"{after}\" is not an ingredient of component \"{component}\""
                        ));
                    }
                },
            };
            doc.components[c]
                .ingredients
                .insert(at, (**ingredient).clone());
        }
        Op::AddStep {
            component,
            after,
            step,
        } => {
            let c = locate_component(doc, component)?;
            let at = match after {
                None => 0,
                Some(after) => match locate(doc, after)? {
                    Slot::Step(owner, s) if owner == c => s + 1,
                    _ => {
                        return Err(format!(
                            "\"{after}\" is not a step of component \"{component}\""
                        ));
                    }
                },
            };
            doc.components[c].steps.insert(at, (**step).clone());
        }
        Op::AddComponent { after, component } => {
            let at = match after {
                None => 0,
                Some(after) => locate_component(doc, after)? + 1,
            };
            doc.components.insert(at, (**component).clone());
        }
        Op::Remove { target } => match locate(doc, target)? {
            Slot::Component(c) => {
                doc.components.remove(c);
            }
            Slot::Ingredient(c, i) => {
                doc.components[c].ingredients.remove(i);
            }
            Slot::Step(c, s) => {
                doc.components[c].steps.remove(s);
            }
        },
        Op::Move { target, after } => move_node(doc, target, after.as_deref())?,
        Op::ReplaceText {
            target,
            find,
            replace,
        } => {
            let step = match locate(doc, target)? {
                Slot::Step(c, s) => &mut doc.components[c].steps[s],
                _ => {
                    return Err(format!(
                        "\"{target}\" is not a step; only step text can be replaced"
                    ));
                }
            };
            if find.is_empty() {
                return Err("find must not be empty".to_owned());
            }
            let count = step.text.matches(find.as_str()).count();
            if count != 1 {
                return Err(format!(
                    "\"{find}\" occurs {count} times in the text of \"{target}\"; it must occur exactly once"
                ));
            }
            step.text = step.text.replace(find.as_str(), replace);
        }
        Op::Rename { target, to } => rename(doc, target, to)?,
        Op::Rescale { factor } => {
            if !(factor.is_finite() && *factor > 0.0) {
                return Err("factor must be a number above 0".to_owned());
            }
            *doc = scaled(doc, *factor);
        }
    }
    Ok(())
}

fn move_node(doc: &mut RecipeDoc, target: &str, after: Option<&str>) -> Result<(), String> {
    let from = locate(doc, target)?;
    let mismatch = |after: &str| {
        let kind = from.kind().as_str();
        let article = if from.kind() == Kind::Ingredient {
            "an"
        } else {
            "a"
        };
        format!(
            "\"{after}\" is not {article} {kind}; \"{target}\" can only move after another {kind}"
        )
    };
    match from {
        Slot::Component(c) => {
            let node = doc.components.remove(c);
            let at = match after {
                None => 0,
                Some(after) => match locate(doc, after)? {
                    Slot::Component(anchor) => anchor + 1,
                    _ => return Err(mismatch(after)),
                },
            };
            doc.components.insert(at, node);
        }
        Slot::Ingredient(c, i) => {
            let node = doc.components[c].ingredients.remove(i);
            let (owner, at) = match after {
                None => (c, 0),
                Some(after) => match locate(doc, after)? {
                    Slot::Ingredient(owner, anchor) => (owner, anchor + 1),
                    _ => return Err(mismatch(after)),
                },
            };
            doc.components[owner].ingredients.insert(at, node);
        }
        Slot::Step(c, s) => {
            let node = doc.components[c].steps.remove(s);
            let (owner, at) = match after {
                None => (c, 0),
                Some(after) => match locate(doc, after)? {
                    Slot::Step(owner, anchor) => (owner, anchor + 1),
                    _ => return Err(mismatch(after)),
                },
            };
            doc.components[owner].steps.insert(at, node);
        }
    }
    Ok(())
}

fn rename(doc: &mut RecipeDoc, old: &str, to: &str) -> Result<(), String> {
    if Index::build(doc).contains(to) {
        return Err(format!("id \"{to}\" is already in use"));
    }
    let byproduct = doc
        .components
        .iter_mut()
        .flat_map(|component| &mut component.steps)
        .flat_map(|step| &mut step.byproducts)
        .find(|byproduct| byproduct.id == old);
    if let Some(byproduct) = byproduct {
        byproduct.id = to.to_owned();
    } else {
        match locate(doc, old)? {
            Slot::Component(c) => doc.components[c].id = to.to_owned(),
            Slot::Ingredient(c, i) => doc.components[c].ingredients[i].id = to.to_owned(),
            Slot::Step(c, s) => doc.components[c].steps[s].id = to.to_owned(),
        }
    }
    for step in doc.components.iter_mut().flat_map(|c| &mut c.steps) {
        for input in step.inputs.iter_mut().filter(|input| input.r#ref == old) {
            input.r#ref = to.to_owned();
        }
        step.text = rename_placeholders(&step.text, old, to);
    }
    Ok(())
}

/// Rewrites `{old}` and `{old|label}` to name `to`.
fn rename_placeholders(text: &str, old: &str, to: &str) -> String {
    let opening = format!("{{{old}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&opening) {
        let after = &rest[at + opening.len()..];
        out.push_str(&rest[..at]);
        if after.starts_with(['|', '}']) {
            out.push('{');
            out.push_str(to);
        } else {
            out.push_str(&opening);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn dumps<T: Serialize>(value: &T) -> String {
    py::json_dumps(&serde_json::to_value(value).unwrap_or(Value::Null))
}

fn fields_of(ix: &Index<'_>, id: &str) -> Map<String, Value> {
    let value = match ix.node(id) {
        Some(Node::Ingredient(node)) => serde_json::to_value(node),
        Some(Node::Step(node)) => serde_json::to_value(node),
        Some(Node::Byproduct(node)) => serde_json::to_value(node),
        Some(Node::Component(node)) => serde_json::to_value(node).map(|mut value| {
            if let Value::Object(map) = &mut value {
                map.shift_remove("ingredients");
                map.shift_remove("steps");
            }
            value
        }),
        None => Ok(Value::Null),
    };
    match value {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Appends an order line when the ids both revisions share changed order.
fn order_change(label: &str, old: &[String], new: &[String], out: &mut Vec<String>) {
    let shared_old: Vec<&String> = old.iter().filter(|id| new.contains(id)).collect();
    let shared_new: Vec<&String> = new.iter().filter(|id| old.contains(id)).collect();
    if shared_old != shared_new {
        out.push(format!("~ {label}: order → {}", new.join(", ")));
    }
}

/// The change list between two revisions, addressed by id: `title: … → …`
/// for the header, `− kind id` and `+ kind id` for removed and added nodes,
/// `~ id.field: old → new` for changed fields, `~ id: moved from a to b` for
/// a node that changed component, and `~ list: order → …` for reordered
/// ingredients, steps or components.
pub fn diff_summary(old: &RecipeDoc, new: &RecipeDoc) -> Vec<String> {
    let (a, b) = (Index::build(old), Index::build(new));
    let mut out = Vec::new();
    let mut header = |key: &str, before: String, after: String| {
        if before != after {
            out.push(format!("{key}: {before} → {after}"));
        }
    };
    header("title", dumps(&old.title), dumps(&new.title));
    header("summary", dumps(&old.summary), dumps(&new.summary));
    header("yield", dumps(&old.r#yield), dumps(&new.r#yield));
    header("time", dumps(&old.time), dumps(&new.time));
    header("source", dumps(&old.source), dumps(&new.source));
    for id in a.ids() {
        if let Some(kind) = a.kind(id).filter(|_| !b.contains(id)) {
            out.push(format!("− {} {id}", kind.as_str()));
        }
    }
    for id in b.ids() {
        let Some(kind) = a.kind(id) else {
            let kind = b.kind(id).map_or("", Kind::as_str);
            out.push(format!("+ {kind} {id}"));
            continue;
        };
        let owner = |ix: &Index<'_>| ix.component_of(id).map(|c| c.id.clone());
        if let (Some(from), Some(to)) = (owner(&a), owner(&b))
            && from != to
            && kind != Kind::Component
        {
            out.push(format!("~ {id}: moved from {from} to {to}"));
        }
        if kind == Kind::Byproduct {
            continue;
        }
        let (before, after) = (fields_of(&a, id), fields_of(&b, id));
        let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let (was, is) = (before.get(name), after.get(name));
            if was != is {
                let show = |value: Option<&Value>| py::json_dumps(value.unwrap_or(&Value::Null));
                out.push(format!("~ {id}.{name}: {} → {}", show(was), show(is)));
            }
        }
    }
    let component_ids = |doc: &'_ RecipeDoc| -> Vec<String> {
        doc.components.iter().map(|c| c.id.clone()).collect()
    };
    order_change(
        "components",
        &component_ids(old),
        &component_ids(new),
        &mut out,
    );
    for component in &new.components {
        let Some(Node::Component(before)) = a.node(&component.id) else {
            continue;
        };
        let ingredient_ids =
            |c: &Component| -> Vec<String> { c.ingredients.iter().map(|i| i.id.clone()).collect() };
        let step_ids =
            |c: &Component| -> Vec<String> { c.steps.iter().map(|s| s.id.clone()).collect() };
        order_change(
            &format!("{}.ingredients", component.id),
            &ingredient_ids(before),
            &ingredient_ids(component),
            &mut out,
        );
        order_change(
            &format!("{}.steps", component.id),
            &step_ids(before),
            &step_ids(component),
            &mut out,
        );
    }
    out
}
