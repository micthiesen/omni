//! The rules a document must satisfy to be saved. Every problem is reported,
//! not only the first, each with a code, a location and, where one can be
//! computed, a hint.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::format::{fmt_amount_exact, is_known_unit, nearest_unit, parse_fraction};
use crate::index::{Index, Kind};
use crate::model::{
    ACTION_MAX, ACTION_WARN, Attention, COMPONENTS_MAX, Component, INGREDIENTS_MAX, Ingredient,
    Input, RecipeDoc, STEPS_MAX, Step, TITLE_MAX, is_valid_id,
};
use crate::py;
use crate::text::{explicit_total, fraction_value, placeholders};

/// The rule a problem breaks. Beyond the rules of the design's table:
/// `bad_value` is a number, fraction, unit or required word that cannot be
/// used as given, `too_long` and `too_many` are the length and size limits,
/// and `missing_name` is an unnamed component in a recipe that has several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Code {
    BadId,
    DuplicateId,
    UnknownRef,
    ForeignRef,
    ForwardRef,
    UnusedIngredient,
    DanglingStep,
    UnusedComponent,
    OverAllocated,
    UnderAllocated,
    AmbiguousSplit,
    DuplicateInput,
    ConflictingAmount,
    NoSteps,
    EmptyAction,
    EmptyText,
    PlaceholderNotInput,
    PlaceholderUnset,
    RefNeedsLabel,
    BadValue,
    TooLong,
    TooMany,
    NoComponents,
    MissingName,
    InputNotMentioned,
    LiteralQuantity,
    LongAction,
    TimeMismatch,
    UnknownUnit,
    MissingTime,
    MissingSourceUrl,
    AwkwardAmount,
    // Reported by the library, not by `validate`: the household diet, which
    // the owner may override for one ingredient.
    DietConflict,
    // The last two are reported by the library on a dry run, not by
    // `validate`. Plain comments: a documented variant would turn the
    // schema's `enum` into a `oneOf`.
    UnknownTag,
    DuplicateSource,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadId => "bad_id",
            Self::DuplicateId => "duplicate_id",
            Self::UnknownRef => "unknown_ref",
            Self::ForeignRef => "foreign_ref",
            Self::ForwardRef => "forward_ref",
            Self::UnusedIngredient => "unused_ingredient",
            Self::DanglingStep => "dangling_step",
            Self::UnusedComponent => "unused_component",
            Self::OverAllocated => "over_allocated",
            Self::UnderAllocated => "under_allocated",
            Self::AmbiguousSplit => "ambiguous_split",
            Self::DuplicateInput => "duplicate_input",
            Self::ConflictingAmount => "conflicting_amount",
            Self::NoSteps => "no_steps",
            Self::EmptyAction => "empty_action",
            Self::EmptyText => "empty_text",
            Self::PlaceholderNotInput => "placeholder_not_input",
            Self::PlaceholderUnset => "placeholder_unset",
            Self::RefNeedsLabel => "ref_needs_label",
            Self::BadValue => "bad_value",
            Self::TooLong => "too_long",
            Self::TooMany => "too_many",
            Self::NoComponents => "no_components",
            Self::MissingName => "missing_name",
            Self::InputNotMentioned => "input_not_mentioned",
            Self::LiteralQuantity => "literal_quantity",
            Self::LongAction => "long_action",
            Self::TimeMismatch => "time_mismatch",
            Self::UnknownUnit => "unknown_unit",
            Self::MissingTime => "missing_time",
            Self::MissingSourceUrl => "missing_source_url",
            Self::AwkwardAmount => "awkward_amount",
            Self::DietConflict => "diet_conflict",
            Self::UnknownTag => "unknown_tag",
            Self::DuplicateSource => "duplicate_source",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An `error` blocks a save; a `warning` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Problem {
    pub code: Code,
    /// Where: `steps.sear.inputs.butter`, `ingredients.flour`, `title`.
    pub path: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    pub level: Level,
}

impl Problem {
    /// `path: [code] message Hint: hint`.
    pub fn line(&self) -> String {
        match &self.hint {
            Some(hint) => format!(
                "{}: [{}] {} Hint: {hint}",
                self.path, self.code, self.message
            ),
            None => format!("{}: [{}] {}", self.path, self.code, self.message),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    pub errors: Vec<Problem>,
    pub warnings: Vec<Problem>,
}

impl Report {
    /// Whether the document may be saved. Warnings do not block a save.
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// The text of a refused save: one numbered line per error, then the
    /// warnings.
    pub fn refusal_text(&self) -> String {
        let mut lines = vec![format!(
            "[invalid_recipe] {} problems; nothing was saved.",
            self.errors.len()
        )];
        for (n, problem) in self.errors.iter().enumerate() {
            lines.push(format!("{}. {}", n + 1, problem.line()));
        }
        if !self.warnings.is_empty() {
            lines.push(format!("{} warnings:", self.warnings.len()));
            for problem in &self.warnings {
                lines.push(format!("- {}", problem.line()));
            }
        }
        lines.join("\n")
    }

    fn error(&mut self, code: Code, path: impl Into<String>, message: impl Into<String>) {
        self.push(Level::Error, code, path, message, None);
    }

    fn error_hint(
        &mut self,
        code: Code,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.push(Level::Error, code, path, message, Some(hint.into()));
    }

    fn warn(
        &mut self,
        code: Code,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) {
        self.push(Level::Warning, code, path, message, Some(hint.into()));
    }

    fn push(
        &mut self,
        level: Level,
        code: Code,
        path: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) {
        let problem = Problem {
            code,
            path: path.into(),
            message: message.into(),
            hint: hint.filter(|hint| !hint.is_empty()),
            level,
        };
        match level {
            Level::Error => self.errors.push(problem),
            Level::Warning => self.warnings.push(problem),
        }
    }
}

/// Checks every rule and returns every problem. Never panics, whatever the
/// document holds.
pub fn validate(doc: &RecipeDoc) -> Report {
    let mut report = Report::default();
    let ix = Index::build(doc);

    check_limits(doc, &mut report);

    for duplicate in ix.duplicates() {
        report.error_hint(
            Code::DuplicateId,
            *duplicate,
            format!("id \"{duplicate}\" is used more than once"),
            "ids are unique across the whole recipe",
        );
    }
    for id in ix.ids() {
        if !is_valid_id(id) {
            report.error(
                Code::BadId,
                *id,
                format!("id \"{id}\" must match ^[a-z][a-z0-9_]{{0,23}}$"),
            );
        }
    }

    let last = doc.components.len().saturating_sub(1);
    for (position, component) in doc.components.iter().enumerate() {
        check_component(&ix, position, component, position == last, &mut report);
    }

    for id in ix.consumed() {
        let uses = ix.consumers(id);
        let split = matches!(
            ix.kind(id),
            Some(Kind::Step | Kind::Component | Kind::Byproduct)
        ) && uses.len() > 1;
        if split && uses.iter().any(|u| !u.input.is_sized()) {
            report.error_hint(
                Code::AmbiguousSplit,
                *id,
                format!(
                    "\"{id}\" is used by {} steps without saying how it is divided",
                    uses.len()
                ),
                "give each use a \"fraction\" (for example \"1/2\"), or mark the last one {\"rest\": true}",
            );
        }
    }

    for (_, step) in ix.steps() {
        if step.time.is_none() {
            report.warn(
                Code::MissingTime,
                format!("steps.{}.time", step.id),
                format!("step \"{}\" has no time", step.id),
                "give every step a time and an attention (active or passive); the totals and the timeline leave it out",
            );
        }
    }
    if doc.source.kind == crate::model::SourceKind::Web
        && doc.source.url.as_deref().is_none_or(str::is_empty)
    {
        report.warn(
            Code::MissingSourceUrl,
            "source.url",
            "a web source has no url",
            "set source.url to the page's address, so the original stays one tap away and duplicates are found",
        );
    }
    for awkward in crate::measure::awkward_amounts(doc) {
        report.warn(
            Code::AwkwardAmount,
            awkward.path,
            awkward.message,
            awkward.hint,
        );
    }
    let active: u64 = ix
        .steps()
        .filter_map(|(_, step)| step.time)
        .filter(|time| time.attention == Attention::Active)
        .map(|time| u64::from(time.longest()))
        .sum();
    let stated = doc.time.map_or(0, |time| u64::from(time.active_min));
    if stated > 0 && active > 0 && (active * 2 < stated || active > stated * 2) {
        report.push(
            Level::Warning,
            Code::TimeMismatch,
            "time.activeMin",
            format!("stated active time is {stated} min but active steps add up to {active} min"),
            None,
        );
    }
    report
}

fn usable(number: f64) -> bool {
    number.is_finite() && number >= 0.0
}

/// The size, length and value limits of the format.
fn check_limits(doc: &RecipeDoc, report: &mut Report) {
    let title = py::len(&doc.title);
    if doc.title.trim().is_empty() {
        report.error(Code::BadValue, "title", "title is required");
    } else if title > TITLE_MAX {
        report.error(
            Code::TooLong,
            "title",
            format!("title is {title} characters; the limit is {TITLE_MAX}"),
        );
    }
    if let Some(url) = &doc.source.url
        && !crate::model::is_web_url(url)
    {
        report.error_hint(
            Code::BadValue,
            "source.url",
            "source.url must be an http:// or https:// address",
            "put anything else (a book page, a person) in source.detail",
        );
    }
    if !(doc.r#yield.qty.is_finite() && doc.r#yield.qty > 0.0) {
        report.error(
            Code::BadValue,
            "yield",
            "yield qty must be a number above 0",
        );
    }
    if doc.r#yield.unit.trim().is_empty() {
        report.error_hint(
            Code::BadValue,
            "yield",
            "yield unit is required",
            "say what the amounts make, for example \"servings\"",
        );
    }
    if let Some(time) = doc.time
        && time.total_min < time.active_min
    {
        report.error(
            Code::BadValue,
            "time",
            format!(
                "totalMin ({}) is less than activeMin ({})",
                time.total_min, time.active_min
            ),
        );
    }
    let count = doc.components.len();
    if count == 0 {
        report.error(
            Code::NoComponents,
            "components",
            "a recipe needs at least one component",
        );
    } else if count > COMPONENTS_MAX {
        report.error(
            Code::TooMany,
            "components",
            format!("a recipe has at most {COMPONENTS_MAX} components; this one has {count}"),
        );
    }
    for component in &doc.components {
        let path = format!("components.{}", component.id);
        let unnamed = component
            .name
            .as_deref()
            .is_none_or(|n| n.trim().is_empty());
        if count > 1 && unnamed {
            report.error_hint(
                Code::MissingName,
                path.as_str(),
                format!("component \"{}\" has no name", component.id),
                "every component needs a name when a recipe has more than one",
            );
        }
        for (what, len, max) in [
            ("ingredients", component.ingredients.len(), INGREDIENTS_MAX),
            ("steps", component.steps.len(), STEPS_MAX),
        ] {
            if len > max {
                report.error(
                    Code::TooMany,
                    path.as_str(),
                    format!("a component has at most {max} {what}; this one has {len}"),
                );
            }
        }
        for ingredient in &component.ingredients {
            let path = format!("ingredients.{}", ingredient.id);
            if ingredient.name.trim().is_empty() {
                report.error(Code::BadValue, path.as_str(), "name is required");
            }
            if ingredient.qty.is_some_and(|qty| !usable(qty)) {
                report.error(
                    Code::BadValue,
                    path.as_str(),
                    "qty must be a number, 0 or more",
                );
            }
            if ingredient.alt.as_ref().is_some_and(|alt| !usable(alt.qty)) {
                report.error(
                    Code::BadValue,
                    format!("{path}.alt"),
                    "qty must be a number, 0 or more",
                );
            }
        }
        for step in &component.steps {
            check_step_limits(step, report);
        }
    }
}

fn check_step_limits(step: &Step, report: &mut Report) {
    let path = format!("steps.{}", step.id);
    let action = py::len(&step.action);
    if action > ACTION_MAX {
        report.error_hint(
            Code::TooLong,
            path.as_str(),
            format!("action is {action} characters; the limit is {ACTION_MAX}"),
            "keep chart labels to a verb or two; details belong in text",
        );
    }
    if let Some(time) = step.time
        && time.max.is_some_and(|max| max < time.min)
    {
        report.error(
            Code::BadValue,
            format!("{path}.time"),
            "max is less than min",
        );
    }
    if let Some(temp) = &step.temp {
        if !temp.value.is_finite() {
            report.error(
                Code::BadValue,
                format!("{path}.temp"),
                "value must be a number",
            );
        }
        if !matches!(temp.unit.as_str(), "F" | "C") {
            report.error(
                Code::BadValue,
                format!("{path}.temp"),
                format!("temp unit \"{}\" must be \"F\" or \"C\"", temp.unit),
            );
        }
    }
    for byproduct in &step.byproducts {
        if byproduct.name.trim().is_empty() {
            report.error(
                Code::BadValue,
                format!("{path}.byproducts.{}", byproduct.id),
                "name is required",
            );
        }
    }
    for input in &step.inputs {
        let path = format!("{path}.inputs.{}", input.r#ref);
        if input.qty.is_some_and(|qty| !usable(qty)) {
            report.error(
                Code::BadValue,
                path.as_str(),
                "qty must be a number, 0 or more",
            );
        }
        if let Some(fraction) = &input.fraction
            && !parse_fraction(fraction).is_some_and(|value| value > 0.0 && value <= 1.0)
        {
            report.error_hint(
                Code::BadValue,
                path.as_str(),
                format!("fraction \"{fraction}\" is not a share between 0 and 1"),
                "write it as a ratio such as \"2/3\"",
            );
        }
    }
}

fn check_component(
    ix: &Index<'_>,
    position: usize,
    component: &Component,
    is_last: bool,
    report: &mut Report,
) {
    let path = format!("components.{}", component.id);
    let steps = &component.steps;
    if steps.is_empty() {
        report.error(Code::NoSteps, path, "a component needs at least one step");
        return;
    }
    let mut local: BTreeSet<&str> = BTreeSet::new();
    local.extend(component.ingredients.iter().map(|i| i.id.as_str()));
    local.extend(steps.iter().map(|s| s.id.as_str()));
    local.extend(
        steps
            .iter()
            .flat_map(|s| s.byproducts.iter().map(|b| b.id.as_str())),
    );
    for (n, step) in steps.iter().enumerate() {
        let step_path = format!("steps.{}", step.id);
        let action = py::len(&step.action);
        if step.action.trim().is_empty() {
            report.error(
                Code::EmptyAction,
                step_path.as_str(),
                "action is required: the short verb phrase shown in the chart",
            );
        } else if action > ACTION_WARN && action <= ACTION_MAX {
            report.warn(
                Code::LongAction,
                step_path.as_str(),
                format!("action \"{}\" is {action} characters", step.action),
                "keep chart labels to a verb or two; details belong in text",
            );
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for input in &step.inputs {
            check_input(
                ix, position, component, &local, step, input, &mut seen, report,
            );
        }
        check_text(ix, step, &step_path, report);
        if n + 1 < steps.len() && ix.consumers(&step.id).is_empty() {
            report.error_hint(
                Code::DanglingStep,
                step_path,
                format!("the result of \"{}\" is never used", step.id),
                "add it to the inputs of a later step; only the last step of a component may be unused",
            );
        }
    }
    if !is_last && ix.consumers(&component.id).is_empty() {
        report.error(
            Code::UnusedComponent,
            path,
            format!(
                "component \"{}\" is never used by a later component",
                component.id
            ),
        );
    }
    for ingredient in &component.ingredients {
        check_allocation(ix, ingredient, report);
        let path = format!("ingredients.{}", ingredient.id);
        check_unit(ingredient.unit.as_deref(), &path, report);
        let alt = ingredient.alt.as_ref().map(|alt| alt.unit.as_str());
        check_unit(alt, &format!("{path}.alt"), report);
    }
    for step in steps {
        for input in &step.inputs {
            let path = format!("steps.{}.inputs.{}", step.id, input.r#ref);
            check_unit(input.unit.as_deref(), &path, report);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_input<'a>(
    ix: &Index<'_>,
    position: usize,
    component: &Component,
    local: &BTreeSet<&str>,
    step: &Step,
    input: &'a Input,
    seen: &mut BTreeSet<&'a str>,
    report: &mut Report,
) {
    let id = input.r#ref.as_str();
    let path = format!("steps.{}.inputs.{id}", step.id);
    let Some(kind) = ix.kind(id) else {
        let hint = py::closest(id, ix.ids().iter().copied())
            .map(|close| format!("did you mean \"{close}\"?"));
        report.push(
            Level::Error,
            Code::UnknownRef,
            path,
            format!("no ingredient, step or component has id \"{id}\""),
            hint,
        );
        return;
    };
    if !seen.insert(id) && input.part.is_none() {
        report.error(
            Code::DuplicateInput,
            path.as_str(),
            format!("\"{id}\" is listed twice in this step"),
        );
    }
    if kind == Kind::Component {
        let defined = ix.doc().components.iter().position(|c| c.id == id);
        if id == component.id || defined.is_none_or(|defined| defined > position) {
            report.error_hint(
                Code::ForwardRef,
                path.as_str(),
                format!("component \"{id}\" is defined after this step"),
                "a component may only use components defined before it",
            );
        }
    } else if !local.contains(id) {
        let owner = ix.component_of(id).map_or("", |c| c.id.as_str());
        report.error_hint(
            Code::ForeignRef,
            path.as_str(),
            format!("\"{id}\" belongs to component \"{owner}\""),
            format!("reference the component itself: {{\"ref\": \"{owner}\"}}"),
        );
    } else if let Some(source) = ix.source_step(id)
        && let (Some(made), Some(used)) = (ix.step_pos(source), ix.step_pos(&step.id))
        && made >= used
    {
        report.error_hint(
            Code::ForwardRef,
            path.as_str(),
            format!("\"{id}\" is produced by step \"{source}\", which comes later"),
            "steps are listed in cooking order; move the producing step earlier",
        );
    }
    let sizes = [input.qty.is_some(), input.fraction.is_some(), input.rest];
    if sizes.into_iter().filter(|set| *set).count() > 1 {
        report.error(
            Code::ConflictingAmount,
            path,
            "use only one of qty, fraction or rest",
        );
    }
}

fn check_unit(unit: Option<&str>, path: &str, report: &mut Report) {
    let Some(unit) = unit.filter(|unit| !unit.is_empty() && !is_known_unit(unit)) else {
        return;
    };
    let hint = match nearest_unit(unit) {
        Some(known) => format!("did you mean \"{known}\"?"),
        None => "use a listed unit, or leave unit out and put the word in the name".to_owned(),
    };
    report.warn(
        Code::UnknownUnit,
        path,
        format!("unit \"{unit}\" is not in the unit list"),
        hint,
    );
}

fn check_allocation(ix: &Index<'_>, ingredient: &Ingredient, report: &mut Report) {
    let id = ingredient.id.as_str();
    let path = format!("ingredients.{id}");
    let uses: Vec<&Input> = ix.consumers(id).iter().map(|u| u.input).collect();
    if uses.is_empty() {
        report.error_hint(
            Code::UnusedIngredient,
            path,
            format!("\"{id}\" is not an input of any step"),
            "add it to the step that uses it, or remove the ingredient",
        );
        return;
    }
    let unit = ingredient.unit.as_deref();
    let Some(total) = ingredient.qty else { return };
    if uses.len() == 1 && uses[0].qty.is_none() {
        return;
    }
    if uses.len() > 1 && uses.iter().any(|u| !u.is_sized()) {
        report.error_hint(
            Code::AmbiguousSplit,
            path,
            format!(
                "\"{id}\" is used by {} steps without saying how it is divided",
                uses.len()
            ),
            "give each use a \"qty\", or mark the last one {\"rest\": true}",
        );
        return;
    }
    let mixed = uses
        .iter()
        .any(|u| u.qty.is_some() && u.unit.as_deref().or(unit) != unit);
    if mixed {
        // Uses in other units are allowed and not summed.
        return;
    }
    let shares: f64 = uses
        .iter()
        .filter_map(|u| u.fraction.as_deref())
        .map(|fraction| total * fraction_value(fraction))
        .sum();
    let used = explicit_total(uses.iter().copied(), unit) + shares;
    let has_rest = uses.iter().any(|u| u.rest);
    let amount = |qty: f64| fmt_amount_exact(Some(qty), unit);
    if has_rest && used >= total - 1e-9 {
        report.error(
            Code::OverAllocated,
            path,
            format!(
                "uses add up to {} of {}, leaving nothing for the \"rest\" use",
                amount(used),
                amount(total)
            ),
        );
    } else if !has_rest && (used - total).abs() > 0.011 * total {
        let code = if used > total {
            Code::OverAllocated
        } else {
            Code::UnderAllocated
        };
        report.error_hint(
            code,
            path,
            format!(
                "uses add up to {} but the ingredient list says {}",
                amount(used),
                amount(total)
            ),
            "fix the amounts, or mark the last use {\"rest\": true}",
        );
    }
}

fn check_text(ix: &Index<'_>, step: &Step, step_path: &str, report: &mut Report) {
    let text = step.text.as_str();
    if text.trim().is_empty() {
        report.error(Code::EmptyText, step_path, "text is required");
        return;
    }
    let path = format!("{step_path}.text");
    let inputs: BTreeSet<&str> = step.inputs.iter().map(|i| i.r#ref.as_str()).collect();
    let mut mentioned: BTreeSet<&str> = BTreeSet::new();
    for placeholder in placeholders(text) {
        let id = placeholder.r#ref;
        if id == "time" || id == "temp" {
            let set = if id == "time" {
                step.time.is_some()
            } else {
                step.temp.is_some()
            };
            if !set {
                report.error(
                    Code::PlaceholderUnset,
                    path.as_str(),
                    format!("{{{id}}} is used but the step has no {id}"),
                );
            }
            continue;
        }
        mentioned.insert(id);
        if !inputs.contains(id) {
            let hint = match py::closest(id, inputs.iter().copied()) {
                Some(close) => format!("did you mean {{{close}}}?"),
                None => "add it to inputs, or write the words without braces".to_owned(),
            };
            report.error_hint(
                Code::PlaceholderNotInput,
                path.as_str(),
                format!("{{{id}}} is not one of this step's inputs"),
                hint,
            );
        } else if placeholder.label.is_none()
            && ix
                .step(id)
                .is_some_and(|target| target.output.as_deref().is_none_or(str::is_empty))
        {
            report.error_hint(
                Code::RefNeedsLabel,
                path.as_str(),
                format!("step \"{id}\" has no output name"),
                format!("name it (\"output\": \"…\") or label the reference: {{{id}|the …}}"),
            );
        }
    }
    for id in inputs.difference(&mentioned) {
        if ix.contains(id) {
            report.warn(
                Code::InputNotMentioned,
                path.as_str(),
                format!("input \"{id}\" has no {{{id}}} placeholder"),
                "amounts written as plain words do not scale and cannot be crossed off",
            );
        }
    }

    let mut nouns: Vec<(String, &str)> = Vec::new();
    for input in &step.inputs {
        let Some(ingredient) = ix.ingredient(&input.r#ref) else {
            continue;
        };
        let Some(last) = ingredient.name.split_whitespace().next_back() else {
            continue;
        };
        let noun = noun_stem(last);
        nouns.retain(|(known, _)| *known != noun);
        nouns.push((noun, input.r#ref.as_str()));
    }
    for literal in literal_quantities(text) {
        let hit = literal.words.iter().enumerate().find_map(|(n, word)| {
            let stem = noun_stem(word);
            nouns
                .iter()
                .find(|(noun, _)| *noun == stem)
                .map(|(_, id)| (n, *id))
        });
        if let Some((n, id)) = hit {
            let mut shown = vec![literal.number, literal.unit];
            shown.extend(&literal.words[..=n]);
            report.warn(
                Code::LiteralQuantity,
                path.as_str(),
                format!(
                    "\"{}\" is a hard-coded amount; it will not scale",
                    shown.join(" ")
                ),
                format!("write {{{id}}} and put the amount on the input"),
            );
        }
    }
}

fn noun_stem(word: &str) -> String {
    word.to_lowercase().trim_end_matches('s').to_owned()
}

/// An amount written into prose: `3 tbsp cold butter`.
struct Literal<'a> {
    number: &'a str,
    unit: &'a str,
    /// The alphabetic words among the four tokens after the unit.
    words: Vec<&'a str>,
}

const LITERAL_UNITS: [&str; 16] = [
    "g",
    "kg",
    "ml",
    "l",
    "tsp",
    "tbsp",
    "cups",
    "cup",
    "teaspoons",
    "teaspoon",
    "tablespoons",
    "tablespoon",
    "oz",
    "lb",
    "grams",
    "gram",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_vulgar(c: char) -> bool {
    matches!(c, '¼' | '½' | '¾' | '⅓' | '⅔' | '⅛')
}

/// A number followed by a measuring unit, with the words after it. A number
/// that follows a word character or an opening brace does not count.
fn literal_quantities(text: &str) -> Vec<Literal<'_>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let offset = |at: usize| chars.get(at).map_or(text.len(), |(byte, _)| *byte);
    let get = |at: usize| chars.get(at).map(|(_, c)| *c);
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let follows_word = at > 0 && get(at - 1).is_some_and(|c| is_word(c) || c == '{');
        let Some(number_end) = (!follows_word).then(|| number_at(&get, at)).flatten() else {
            at += 1;
            continue;
        };
        let mut unit_start = number_end;
        while get(unit_start).is_some_and(char::is_whitespace) {
            unit_start += 1;
        }
        let Some(unit_end) = unit_at(&get, unit_start) else {
            at += 1;
            continue;
        };
        let mut end = unit_end;
        for _ in 0..4 {
            let mut next = end;
            while get(next).is_some_and(char::is_whitespace) {
                next += 1;
            }
            if next == end || get(next).is_none() {
                break;
            }
            while get(next).is_some_and(|c| !c.is_whitespace()) {
                next += 1;
            }
            end = next;
        }
        let tail = &text[offset(unit_end)..offset(end)];
        found.push(Literal {
            number: &text[offset(at)..offset(number_end)],
            unit: &text[offset(unit_start)..offset(unit_end)],
            words: tail
                .split(|c: char| !(c.is_ascii_alphabetic() || c == 'é'))
                .filter(|word| !word.is_empty())
                .collect(),
        });
        at = end.max(at + 1);
    }
    found
}

/// The end of `\d+(?:[./]\d+)?` or one vulgar fraction starting at `at`.
fn number_at(get: &impl Fn(usize) -> Option<char>, at: usize) -> Option<usize> {
    let digits = |mut at: usize| {
        while get(at).is_some_and(|c| c.is_ascii_digit()) {
            at += 1;
        }
        at
    };
    let first = get(at)?;
    if is_vulgar(first) {
        return Some(at + 1);
    }
    let end = digits(at);
    if end == at {
        return None;
    }
    if matches!(get(end), Some('.' | '/')) {
        let fraction_end = digits(end + 1);
        if fraction_end > end + 1 {
            return Some(fraction_end);
        }
    }
    Some(end)
}

/// The end of a measuring unit at `at` that is not followed by a word
/// character.
fn unit_at(get: &impl Fn(usize) -> Option<char>, at: usize) -> Option<usize> {
    LITERAL_UNITS.iter().find_map(|unit| {
        let matches = unit
            .chars()
            .enumerate()
            .all(|(n, want)| get(at + n).is_some_and(|c| c.to_ascii_lowercase() == want));
        let end = at + unit.len();
        (matches && !get(end).is_some_and(is_word)).then_some(end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(text: &str) -> Vec<String> {
        literal_quantities(text)
            .iter()
            .map(|l| format!("{}|{}|{}", l.number, l.unit, l.words.join(" ")))
            .collect()
    }

    #[test]
    fn literal_quantities_need_a_number_and_a_measuring_unit() {
        assert_eq!(
            shown("swirl in 3 tbsp cold butter. Taste and adjust"),
            ["3|tbsp|cold butter Taste and"]
        );
        assert_eq!(shown("add ½ Cup flour"), ["½|Cup|flour"]);
        // The four tokens after the unit belong to the first amount.
        assert_eq!(
            shown("add 1.5 cups of sugar, then 2g salt"),
            ["1.5|cups|of sugar then g"]
        );
        assert_eq!(
            shown("then 2g salt and 1/2 tsp\nfine pepper"),
            ["2|g|salt and tsp"]
        );
        assert_eq!(shown("3 to 4 minutes per side"), Vec::<String>::new());
        assert_eq!(shown("a 23 x 33 cm baking dish"), Vec::<String>::new());
        assert_eq!(shown("use {x2 g} or abc3 g"), Vec::<String>::new());
        assert_eq!(shown("4 lemons and 5 grams"), ["5|grams|"]);
        assert_eq!(shown("2 l"), ["2|l|"]);
    }
}
