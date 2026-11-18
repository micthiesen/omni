//! How one input reads, and step text with its placeholders filled in.

use serde::{Deserialize, Serialize};

use crate::format::{
    fmt_amount, fmt_number, fmt_temp, fmt_time, fmt_time_prose, inflect, parse_fraction,
};
use crate::index::{Index, Kind, Node};
use crate::model::{Attention, Id, Input, Step};

/// A `{ref}` or `{ref|label}` placeholder in step text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholder<'a> {
    /// Byte range of the placeholder, braces included.
    pub start: usize,
    pub end: usize,
    pub r#ref: &'a str,
    /// The words after `|`, when there are any.
    pub label: Option<&'a str>,
}

/// Every placeholder in `text`, in order. Braces do not nest: an opening
/// brace restarts the placeholder.
pub fn placeholders(text: &str) -> Vec<Placeholder<'_>> {
    let mut found = Vec::new();
    let mut open = None;
    for (at, byte) in text.bytes().enumerate() {
        match byte {
            b'{' => open = Some(at),
            b'}' => {
                if let Some(start) = open.take() {
                    let inner = &text[start + 1..at];
                    let (r#ref, label) = match inner.split_once('|') {
                        Some((r#ref, label)) => (r#ref, Some(label).filter(|l| !l.is_empty())),
                        None => (inner, None),
                    };
                    found.push(Placeholder {
                        start,
                        end: at + 1,
                        r#ref,
                        label,
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// The amount one use of an ingredient takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UseQty<'a> {
    pub qty: Option<f64>,
    pub unit: Option<&'a str>,
    /// The use takes whatever the other uses leave.
    pub rest: bool,
}

/// The sum of explicitly sized uses in `unit`. Uses naming different parts of
/// the same items (zest and juice of one lemon) overlap, so parts count once,
/// at the largest per-part total.
pub(crate) fn explicit_total<'a>(uses: impl Iterator<Item = &'a Input>, unit: Option<&str>) -> f64 {
    let mut plain = 0.0;
    let mut parts: Vec<(&str, f64)> = Vec::new();
    for input in uses {
        let Some(qty) = input.qty else { continue };
        match &input.part {
            Some(part) => match parts.iter_mut().find(|(name, _)| name == part) {
                Some((_, total)) => *total += qty,
                None => parts.push((part, qty)),
            },
            None => {
                if input.unit.as_deref().or(unit) == unit {
                    plain += qty;
                }
            }
        }
    }
    plain
        + parts
            .iter()
            .map(|(_, total)| *total)
            .reduce(f64::max)
            .unwrap_or(0.0)
}

/// The scaled amount for one use of an ingredient. `input` must be borrowed
/// from the indexed document: a `rest` use is told apart from the other uses
/// by address.
pub fn use_qty<'a>(ix: &'a Index<'_>, input: &'a Input, factor: f64) -> UseQty<'a> {
    let Some(ingredient) = ix.ingredient(&input.r#ref) else {
        return UseQty {
            qty: None,
            unit: None,
            rest: input.rest,
        };
    };
    let factor = if ingredient.scales { factor } else { 1.0 };
    let unit = ingredient.unit.as_deref();
    if let Some(qty) = input.qty {
        return UseQty {
            qty: Some(qty * factor),
            unit: input.unit.as_deref().or(unit),
            rest: false,
        };
    }
    let Some(total) = ingredient.qty else {
        return UseQty {
            qty: None,
            unit,
            rest: input.rest,
        };
    };
    let share = |input: &Input| input.fraction.as_deref().map(|f| fraction_value(f) * total);
    if let Some(share) = share(input) {
        return UseQty {
            qty: Some(share * factor),
            unit,
            rest: false,
        };
    }
    if input.rest {
        let others = || {
            ix.consumers(&input.r#ref)
                .iter()
                .map(|other| other.input)
                .filter(|other| !std::ptr::eq(*other, input))
        };
        let used = explicit_total(others(), unit) + others().filter_map(share).sum::<f64>();
        return UseQty {
            qty: Some((total - used) * factor),
            unit,
            rest: true,
        };
    }
    UseQty {
        qty: Some(total * factor),
        unit,
        rest: false,
    }
}

pub(crate) fn fraction_value(fraction: &str) -> f64 {
    parse_fraction(fraction).unwrap_or(0.0)
}

/// How one input reads: in prose (step text) or as a row label (chart, cook
/// mode). `input` must be borrowed from the indexed document.
pub fn use_label(ix: &Index<'_>, input: &Input, factor: f64, prose: bool) -> String {
    let id = input.r#ref.as_str();
    let name = match ix.node(id) {
        Some(Node::Ingredient(ingredient)) => {
            let used = use_qty(ix, input, factor);
            let name = inflect(&ingredient.name, used.qty, used.unit, ingredient);
            let amount = fmt_amount(used.qty, used.unit);
            if let Some(part) = &input.part {
                let core = format!("{part} of {amount} {name}").replace("  ", " ");
                return if prose { format!("the {core}") } else { core };
            }
            let mut core = format!("{amount} {name}").trim().to_owned();
            if used.rest && prose {
                core = format!("the remaining {core}");
            }
            let own_prep = input.prep.as_deref().filter(|prep| !prep.is_empty());
            let listed_prep = ingredient
                .prep
                .as_deref()
                .filter(|prep| !prose && !prep.is_empty());
            if let Some(prep) = own_prep.or(listed_prep) {
                core.push_str(", ");
                core.push_str(prep);
            }
            if used.rest && !prose {
                core.push_str(" (the rest)");
            }
            if !prose && ingredient.optional {
                core.push_str(" (optional)");
            }
            return core;
        }
        Some(Node::Component(component)) => component.name.as_deref().unwrap_or(id).to_lowercase(),
        Some(Node::Byproduct(byproduct)) => byproduct.name.clone(),
        Some(Node::Step(step)) => step
            .output
            .as_deref()
            .filter(|output| !output.is_empty())
            .unwrap_or(id)
            .to_owned(),
        None => id.to_owned(),
    };
    if let Some(fraction) = &input.fraction {
        format!(
            "{} of the {name}",
            fmt_number(fraction_value(fraction), None)
        )
    } else if input.rest {
        if prose {
            format!("the remaining {name}")
        } else {
            format!("rest of the {name}")
        }
    } else if prose {
        format!("the {name}")
    } else {
        name
    }
}

/// One piece of rendered step text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Segment {
    /// Words as written.
    Text { text: String },
    /// A reference to one of the step's inputs.
    Input {
        text: String,
        r#ref: Id,
        /// The cook-mode key of the use: `step/ref`, or `step/ref:part`.
        key: String,
        /// What the ref names; absent when it resolves to nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Kind>,
    },
    /// The step's time.
    Time { text: String },
    /// The step's temperature.
    Temp { text: String },
}

impl Segment {
    pub fn text(&self) -> &str {
        match self {
            Self::Text { text }
            | Self::Input { text, .. }
            | Self::Time { text }
            | Self::Temp { text } => text,
        }
    }
}

/// Step text split at its placeholders, so a viewer can set amounts in bold
/// and make inputs tappable. A placeholder that cannot be filled (not an
/// input, or no time or temperature on the step) stays as written.
pub fn render_step_segments(ix: &Index<'_>, step: &Step, factor: f64) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut plain = String::new();
    let mut at = 0;
    for placeholder in placeholders(&step.text) {
        plain.push_str(&step.text[at..placeholder.start]);
        at = placeholder.end;
        let filled = match placeholder.r#ref {
            "time" => step.time.map(|time| Segment::Time {
                text: fmt_time_prose(time.min, time.max),
            }),
            "temp" => step.temp.as_ref().map(|temp| Segment::Temp {
                text: fmt_temp(temp, true),
            }),
            id => step
                .inputs
                .iter()
                .find(|input| input.r#ref == id)
                .map(|input| Segment::Input {
                    text: match placeholder.label {
                        Some(label) => label.to_owned(),
                        None => use_label(ix, input, factor, true),
                    },
                    r#ref: input.r#ref.clone(),
                    key: input.key(&step.id),
                    target: ix.kind(id),
                }),
        };
        match filled {
            Some(segment) => {
                if !plain.is_empty() {
                    segments.push(Segment::Text {
                        text: std::mem::take(&mut plain),
                    });
                }
                segments.push(segment);
            }
            None => match placeholder.label {
                Some(label) => plain.push_str(label),
                None => plain.push_str(&step.text[placeholder.start..placeholder.end]),
            },
        }
    }
    plain.push_str(&step.text[at..]);
    if !plain.is_empty() {
        segments.push(Segment::Text { text: plain });
    }
    segments
}

/// Step text with every placeholder filled in at `factor`.
pub fn render_text(ix: &Index<'_>, step: &Step, factor: f64) -> String {
    render_step_segments(ix, step, factor)
        .iter()
        .map(Segment::text)
        .collect()
}

/// The chart cell of a step: its action, its temperature, and its time when
/// the step is passive or takes three minutes or more.
pub fn chart_label(step: &Step) -> String {
    let mut label = step.action.clone();
    if let Some(temp) = &step.temp {
        label.push(' ');
        label.push_str(&fmt_temp(temp, false));
    }
    if let Some(time) = step.time
        && (time.attention == Attention::Passive || time.min >= 3)
    {
        label.push(' ');
        label.push_str(&fmt_time(time.min, time.max));
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_do_not_nest() {
        let found = placeholders("a {x} b {y|the y} {{z} } {|} {w|}");
        let refs: Vec<_> = found.iter().map(|p| (p.r#ref, p.label)).collect();
        assert_eq!(
            refs,
            [
                ("x", None),
                ("y", Some("the y")),
                ("z", None),
                ("", None),
                ("w", None)
            ]
        );
        assert_eq!(&"a {x} b"[found[0].start..found[0].end], "{x}");
    }
}
