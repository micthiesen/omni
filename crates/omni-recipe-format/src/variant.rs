//! Named variants of one recipe: each is a short list of edit operations on
//! the recipe as written, and every view of a variant is the plain document
//! those operations make, so the renderers, the validator and the household
//! checks run on it unchanged.

use serde_json::Value;

use crate::index::Index;
use crate::model::{RecipeDoc, Variant};
use crate::ops::{Op, OpError, VariantOp, apply_ops};

pub const VARIANTS_MAX: usize = 6;
pub const VARIANT_OPS_MAX: usize = 50;
pub const VARIANT_NAME_MAX: usize = 60;

/// Why a variant's document could not be made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VariantError {
    #[error("the recipe has no variant \"{id}\"; its variants are: {}", known.join(", "))]
    Unknown { id: String, known: Vec<String> },
    #[error("ops[{index}] ({op}): {reason}")]
    Refused {
        index: usize,
        op: &'static str,
        reason: String,
    },
    #[error(transparent)]
    Op(#[from] OpError),
}

/// Why `op` cannot be part of a variant; None when it can. A variant's
/// `setRecipe` changes only the yield and the time.
pub(crate) fn refused(op: &VariantOp) -> Option<String> {
    let VariantOp::SetRecipe { fields, unset } = op else {
        return None;
    };
    fields
        .keys()
        .chain(unset.iter())
        .find(|key| !matches!(key.as_str(), "yield" | "time"))
        .map(|key| format!("a variant changes only the yield and time, not \"{key}\""))
}

/// The recipe as `variant` makes it, with no variants of its own.
pub fn apply_variant(doc: &RecipeDoc, variant: &Variant) -> Result<RecipeDoc, VariantError> {
    for (index, op) in variant.ops.iter().enumerate() {
        if let Some(reason) = refused(op) {
            return Err(VariantError::Refused {
                index,
                op: "setRecipe",
                reason,
            });
        }
    }
    let mut base = doc.clone();
    base.variants.clear();
    let ops: Vec<Op> = variant.ops.iter().map(VariantOp::to_op).collect();
    Ok(apply_ops(&base, &ops)?)
}

/// The recipe as the variant `id` makes it.
pub fn variant_doc(doc: &RecipeDoc, id: &str) -> Result<RecipeDoc, VariantError> {
    match doc.variants.iter().find(|variant| variant.id == id) {
        Some(variant) => apply_variant(doc, variant),
        None => Err(VariantError::Unknown {
            id: id.to_owned(),
            known: doc.variants.iter().map(|v| v.id.clone()).collect(),
        }),
    }
}

/// Every variant that applies, with its document, in written order.
pub fn variant_docs(doc: &RecipeDoc) -> Vec<(&Variant, RecipeDoc)> {
    doc.variants
        .iter()
        .filter_map(|variant| apply_variant(doc, variant).ok().map(|made| (variant, made)))
        .collect()
}

/// A number as JSON: whole numbers as integers, so `300` stays `300`.
fn json_number(x: f64) -> Option<Value> {
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        #[allow(clippy::cast_possible_truncation)]
        return Some(Value::from(x as i64));
    }
    serde_json::Number::from_f64(x).map(Value::Number)
}

/// What `made` changes of `doc`: "yield" and "time" when those differ, then
/// the id of every ingredient, step and component whose own fields differ,
/// in written order.
pub fn changed_ids(doc: &RecipeDoc, made: &RecipeDoc) -> Vec<String> {
    let mut out = Vec::new();
    if doc.r#yield != made.r#yield {
        out.push("yield".to_owned());
    }
    if doc.time != made.time {
        out.push("time".to_owned());
    }
    let before = Index::build(doc);
    for component in &made.components {
        let own = |c: &crate::model::Component| (c.name.clone(), c.make_ahead.clone());
        if before
            .doc()
            .components
            .iter()
            .find(|known| known.id == component.id)
            .is_none_or(|known| own(known) != own(component))
        {
            out.push(component.id.clone());
        }
        for ingredient in &component.ingredients {
            if before.ingredient(&ingredient.id) != Some(ingredient) {
                out.push(ingredient.id.clone());
            }
        }
        for step in &component.steps {
            if before.step(&step.id) != Some(step) {
                out.push(step.id.clone());
            }
        }
    }
    out
}

fn scale_number(value: &mut Value, factor: f64) {
    if let Some(scaled) = value
        .as_f64()
        .and_then(|number| json_number(number * factor))
    {
        *value = scaled;
    }
}

/// `ops` with the amounts they set multiplied by `factor`, as the `rescale`
/// operation does to the recipe itself: ingredient totals and `alt`, input
/// amounts, portions, batch counts and the yield. An ingredient that does not
/// scale keeps its amounts.
pub(crate) fn scale_ops(doc: &RecipeDoc, ops: &mut [VariantOp], factor: f64) {
    let ix = Index::build(doc);
    let scales = |id: &str| ix.ingredient(id).is_none_or(|ingredient| ingredient.scales);
    for op in ops {
        match op {
            VariantOp::Set { target, fields, .. } => {
                if ix.ingredient(target).is_some() {
                    let own = fields.get("scales").and_then(Value::as_bool);
                    if own.unwrap_or_else(|| scales(target)) {
                        if let Some(qty) = fields.get_mut("qty") {
                            scale_number(qty, factor);
                        }
                        if let Some(alt) = fields.get_mut("alt").and_then(|alt| alt.get_mut("qty"))
                        {
                            scale_number(alt, factor);
                        }
                    }
                } else if ix.step(target).is_some() {
                    if let Some(qty) = fields.get_mut("portions").and_then(|p| p.get_mut("qty")) {
                        scale_number(qty, factor);
                    }
                    if let Some(count) = fields.get_mut("repeat").and_then(|r| r.get_mut("count"))
                        && let Some(rounded) = count.as_f64().and_then(|number| {
                            json_number((number * factor - 1e-9).ceil().max(1.0))
                        })
                    {
                        *count = rounded;
                    }
                }
            }
            VariantOp::SetInput { r#ref, fields, .. } if scales(r#ref) => {
                if let Some(qty) = fields.get_mut("qty") {
                    scale_number(qty, factor);
                }
            }
            VariantOp::SetRecipe { fields, .. } => {
                if let Some(qty) = fields.get_mut("yield").and_then(|y| y.get_mut("qty")) {
                    scale_number(qty, factor);
                }
            }
            _ => {}
        }
    }
}
