//! The recipe document: what is stored, versioned and exchanged with agents.
//!
//! camelCase on the wire, unknown fields refused, absent optional fields
//! omitted (never `null`).

use serde::{Deserialize, Serialize, Serializer};

/// The format version written with every stored document.
pub const FORMAT: u32 = 1;

/// A short id chosen by the author, unique across the recipe:
/// `^[a-z][a-z0-9_]{0,23}$`.
pub type Id = String;

pub const TITLE_MAX: usize = 120;
pub const COMPONENTS_MAX: usize = 8;
pub const ACTION_MAX: usize = 40;
/// Chart labels longer than this draw a warning.
pub const ACTION_WARN: usize = 28;
pub const INGREDIENTS_MAX: usize = 100;
pub const STEPS_MAX: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecipeDoc {
    /// 1 to 120 characters.
    pub title: String,
    /// One or two sentences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// What the amounts make; the base for scaling.
    pub r#yield: Yield,
    /// Only when the step times cannot express it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<TimeOverride>,
    pub source: Source,
    /// 1 to 8, in cooking order.
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Yield {
    #[serde(serialize_with = "number")]
    pub qty: f64,
    /// "servings", "cookies", "loaf".
    pub unit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimeOverride {
    pub active_min: u32,
    pub total_min: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Web,
    Book,
    Person,
    Original,
    Crouton,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Component {
    pub id: Id,
    /// Required when the recipe has more than one component.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make_ahead: Option<MakeAhead>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingredients: Vec<Ingredient>,
    /// At least one, in cooking order; the last step's result is the component.
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MakeAhead {
    /// "3 days".
    pub keeps: String,
    /// "fridge".
    pub r#where: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ingredient {
    pub id: Id,
    /// The total to shop and gather; absent for "to taste" items.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_number"
    )]
    pub qty: Option<f64>,
    /// g kg ml l tsp tbsp cup oz lb, or a count noun; absent for a plain count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub name: String,
    /// Only for irregular count nouns ("bay leaves").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plural: Option<String>,
    /// The singular of an irregular `plural` when `name` is the plural form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub singular: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prep: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The source's other measure: 60 g with alt ½ cup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<Amount>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub to_taste: bool,
    /// Default true; false for amounts that do not follow the yield ("butter
    /// for the dish").
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub scales: bool,
    /// This ingredient is another recipe ("1 batch pizza dough").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Amount {
    #[serde(serialize_with = "number")]
    pub qty: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    pub id: Id,
    /// 1 to 40 characters, verb first: the chart cell.
    pub action: String,
    /// The edges of the graph; empty for setup steps such as preheating.
    pub inputs: Vec<Input>,
    /// Prose with `{id}`, `{id|label}`, `{time}`, `{temp}`, `{yield}`,
    /// `{portions}`, `{size}` and `{size:kind}` placeholders.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<StepTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp: Option<Temp>,
    /// What the step divides the food into, following the scale: `{portions}`
    /// renders "16 pieces" at 1 and "24 pieces" at 1.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portions: Option<Portions>,
    /// Sizes the step works to: rolling thickness, cutter diameter, pan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub size: Vec<Size>,
    /// Doneness cue: "edges golden, centres soft".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub equipment: Vec<String>,
    /// How later steps refer to the result: "seared cutlets".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub byproducts: Vec<Byproduct>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
}

/// Whether a step keeps the cook busy (`active`) or runs unattended once
/// started (`passive`).
// Variants carry no doc comments: schemars would turn each documented variant
// into its own `const` alternative instead of one string `enum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum Attention {
    Active,
    Passive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepTime {
    /// Minutes.
    pub min: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    pub attention: Attention,
}

impl StepTime {
    /// The minutes a schedule should allow for the step.
    pub fn longest(&self) -> u32 {
        self.max.unwrap_or(self.min)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Temp {
    #[serde(serialize_with = "number")]
    pub value: f64,
    /// "F" or "C".
    pub unit: String,
}

pub const SIZES_MAX: usize = 4;

/// What a size measures. Variants carry no doc comments (see [`Attention`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum SizeKind {
    Thickness,
    Diameter,
    Length,
    Pan,
}

impl SizeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thickness => "thickness",
            Self::Diameter => "diameter",
            Self::Length => "length",
            Self::Pan => "pan",
        }
    }
}

/// A dimension: "3–4 mm" thick, a "7 cm" cutter, a "23 × 33 cm" pan. Sizes
/// never scale; a pan size is for the recipe as written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Size {
    pub kind: SizeKind,
    #[serde(serialize_with = "number")]
    pub value: f64,
    /// The top of a range: 3 to 4 mm.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_number"
    )]
    pub max: Option<f64>,
    /// The second side of a pan or a length: 23 by 33 cm.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_number"
    )]
    pub by: Option<f64>,
    /// "mm", "cm" or "in".
    pub unit: String,
}

/// A count a step divides the food into: "16 pieces".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Portions {
    #[serde(serialize_with = "number")]
    pub qty: f64,
    /// "pieces", "rounds", "wedges".
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Byproduct {
    pub id: Id,
    /// "pan drippings".
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    /// An ingredient, step, by-product or component id.
    pub r#ref: Id,
    /// At most one of `qty`, `fraction` and `rest`; none means all of it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "optional_number"
    )]
    pub qty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// "2/3".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<String>,
    /// Whatever the other uses leave.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rest: bool,
    /// "juice", "zest", "whites".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    /// Prep for this use only: "cut into wedges".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prep: Option<String>,
}

impl Input {
    /// An input that takes all of `id`.
    pub fn all(id: impl Into<Id>) -> Self {
        Self {
            r#ref: id.into(),
            qty: None,
            unit: None,
            fraction: None,
            rest: false,
            part: None,
            prep: None,
        }
    }

    /// Whether the use says how much it takes.
    pub fn is_sized(&self) -> bool {
        self.rest || self.qty.is_some() || self.fraction.is_some()
    }

    /// The key cook mode and the chart share for this use within `step`:
    /// `step/ref`, or `step/ref:part`.
    pub fn key(&self, step: &str) -> String {
        match &self.part {
            Some(part) => format!("{step}/{}:{part}", self.r#ref),
            None => format!("{step}/{}", self.r#ref),
        }
    }
}

/// Whether `url` is a plain web address: `http://` or `https://`, a host,
/// and no whitespace or control characters. Anything else (`javascript:`,
/// `data:`, a bare path) is never stored as a source link nor rendered as one.
pub fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        rest.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '[')
    }) && url.len() <= 2000
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Whether `id` matches `^[a-z][a-z0-9_]{0,23}$`.
pub fn is_valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && id.len() <= 24
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Why a stored document could not be read.
#[derive(Debug, thiserror::Error)]
pub enum UpgradeError {
    #[error("recipe format {found} is not supported; this build reads up to format {FORMAT}")]
    UnsupportedFormat { found: u32 },
    #[error("the stored recipe does not match format {format}: {source}")]
    Invalid {
        format: u32,
        source: serde_json::Error,
    },
}

/// Reads a document stored under `format`, converting older formats to the
/// current one.
pub fn upgrade(format: u32, value: serde_json::Value) -> Result<RecipeDoc, UpgradeError> {
    match format {
        FORMAT => {
            serde_json::from_value(value).map_err(|source| UpgradeError::Invalid { format, source })
        }
        found => Err(UpgradeError::UnsupportedFormat { found }),
    }
}

/// Whole numbers are written as integers, so `4` stays `4` and not `4.0`.
fn number<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    if value.fract() == 0.0 && value.abs() < EXACT {
        #[allow(clippy::cast_possible_truncation)]
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

fn optional_number<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => number(value, serializer),
        None => serializer.serialize_none(),
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_true(value: &bool) -> bool {
    *value
}

fn yes() -> bool {
    true
}
