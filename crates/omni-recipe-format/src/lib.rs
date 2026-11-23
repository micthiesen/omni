//! Fond's recipe format: the document types, the validator, the list and
//! chart renderings, scaling, edit operations, the timeline and the search
//! tokeniser. Pure: no I/O and no clock, so the server and the wasm app run
//! the same code.

pub mod amount;
pub mod chart;
pub mod cook;
pub mod diet;
pub mod format;
pub mod index;
pub mod list;
pub mod measure;
pub mod model;
pub mod ops;
mod py;
pub mod search;
pub mod text;
pub mod timeline;
mod unscaled;
pub mod validate;
pub mod variant;

pub use amount::{
    AmountError, MAX_QTY, amount_ops, amount_summary, parse_qty, qty_text, unit_choices,
};
pub use chart::{
    Block, Cell, CellKind, Mark, Marks, chart_ascii, chart_blocks, chart_html, cook_marks,
};
pub use cook::{
    InputLabel, depends_on, equipment, gather_list, meanwhile, ready_steps, step_inputs,
    step_sources,
};
pub use diet::{DietFlag, DietKind, diet_flag, diet_problems};
pub use index::{Index, Kind, Node, Use};
pub use list::{
    IngredientGroup, IngredientLine, ListOptions, ListStep, ListView, ingredient_groups,
    ingredient_line, list_view, render_list, render_list_with,
};
pub use model::{
    Amount, Attention, Byproduct, Component, FORMAT, HeatLevel, HeatStage, Id, Ingredient, Input,
    MakeAhead, Portions, RecipeDoc, Repeat, Size, SizeKind, Source, SourceKind, Step, StepTime,
    Temp, TimeOverride, UpgradeError, Variant, Yield, is_valid_id, is_web_url, upgrade,
};
pub use ops::{
    Fields, Op, OpError, VariantOp, apply_ops, diff_summary, diff_summary_with_ops, scaled,
};
pub use search::{fold, matches, search_text, tokens};
pub use text::{
    Placeholder, Segment, UseQty, chart_label, chart_label_at, placeholders, render_step_segments,
    render_text, use_label, use_qty,
};
pub use timeline::{
    DerivedTime, TimelineRow, derived_time, derived_time_at, render_timeline, timeline, timeline_at,
};
pub use validate::{Code, Level, Problem, Report, validate};
pub use variant::{VariantError, changed_ids, variant_doc, variant_docs};
