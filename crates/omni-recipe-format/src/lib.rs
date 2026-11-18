//! Fond's recipe format: the document types, the validator, the list and
//! chart renderings, scaling, edit operations, the timeline and the search
//! tokeniser. Pure: no I/O and no clock, so the server and the wasm app run
//! the same code.

pub mod chart;
pub mod cook;
pub mod format;
pub mod index;
pub mod list;
pub mod model;
pub mod ops;
mod py;
pub mod search;
pub mod text;
pub mod timeline;
pub mod validate;

pub use chart::{
    Block, Cell, CellKind, Mark, Marks, chart_ascii, chart_blocks, chart_html, cook_marks,
};
pub use cook::{
    InputLabel, depends_on, equipment, gather_list, meanwhile, ready_steps, step_inputs,
    step_sources,
};
pub use index::{Index, Kind, Node, Use};
pub use list::{
    IngredientGroup, IngredientLine, ListStep, ListView, ingredient_groups, ingredient_line,
    list_view, render_list,
};
pub use model::{
    Amount, Attention, Byproduct, Component, FORMAT, Id, Ingredient, Input, MakeAhead, RecipeDoc,
    Source, SourceKind, Step, StepTime, Temp, TimeOverride, UpgradeError, Yield, is_valid_id,
    is_web_url, upgrade,
};
pub use ops::{Fields, Op, OpError, apply_ops, diff_summary, diff_summary_with_ops, scaled};
pub use search::{fold, matches, search_text, tokens};
pub use text::{
    Placeholder, Segment, UseQty, chart_label, placeholders, render_step_segments, render_text,
    use_label, use_qty,
};
pub use timeline::{DerivedTime, TimelineRow, derived_time, render_timeline, timeline};
pub use validate::{Code, Level, Problem, Report, validate};
