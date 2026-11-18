//! The chart as an HTML table. The layout is `omni_recipe_format::chart_blocks`;
//! this only turns cell rectangles into `rowspan`/`colspan` and adds taps.

use std::sync::Arc;

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use omni_recipe_format::{Cell, CellKind, RecipeDoc, chart_blocks};

use crate::logic::label_columns;
use crate::platform::window;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellMark {
    None,
    Done,
    Ready,
}

/// A viewport with room for charts to read horizontally: a phone on its side,
/// or anything at least 700px wide.
fn viewport_is_wide() -> bool {
    let size = |value: Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>| {
        value.ok().and_then(|value| value.as_f64()).unwrap_or(0.0)
    };
    let (width, height) = (size(window().inner_width()), size(window().inner_height()));
    width > height || width >= 700.0
}

fn span(count: usize) -> Option<String> {
    (count > 1).then(|| count.to_string())
}

fn cell_view(
    cell: &Cell,
    mark: Option<Callback<Cell, CellMark>>,
    on_tap: Callback<Cell>,
    leaf_taps: bool,
) -> Vec<AnyView> {
    let rows = cell.r1 - cell.r0 + 1;
    let cols = cell.c1 - cell.c0 + 1;
    let is_step = cell.kind == CellKind::Step;
    let tappable = leaf_taps || matches!(cell.kind, CellKind::Step | CellKind::Setup);
    // An input row that stretches right is drawn as its label in the sticky
    // first column plus an empty stretch, so the label stays put while the
    // table scrolls sideways.
    let split = !is_step && cell.c0 == 0 && cols > 1;
    // Tall step cells set their text vertically, as the original notation
    // does, which keeps wide charts narrow.
    let columns = is_step.then(|| label_columns(&cell.label, rows)).flatten();
    let vertical = columns.is_some();
    let td = |lead: bool, filler: bool| {
        let class = format!(
            "{}{}{}{}{}",
            cell.kind.as_str(),
            if cell.optional { " optional" } else { "" },
            if lead { " lead" } else { "" },
            if filler { " fill" } else { "" },
            if vertical { " vertical" } else { "" },
        );
        let (marked, tapped) = (cell.clone(), cell.clone());
        let state = move || mark.map_or(CellMark::None, |mark| mark.run(marked.clone()));
        let (done, ready) = (state.clone(), state);
        let colspan = if filler {
            span(cols - 1)
        } else if split {
            None
        } else {
            span(cols)
        };
        view! {
            <td
                class=class
                class:done=move || done() == CellMark::Done
                class:ready=move || ready() == CellMark::Ready
                class:tap=tappable
                data-key=cell.key.clone()
                style=format!("--rows:{rows}")
                rowspan=span(rows)
                colspan=colspan
                on:click=move |_| {
                    if tappable {
                        on_tap.run(tapped.clone());
                    }
                }
            >
                {(!filler).then(|| match &columns {
                    // The spaces between columns keep the cell's text equal to
                    // its label, and separate the words when it lies flat.
                    Some(columns) => view! {
                        <span class="cols">
                            {columns
                                .iter()
                                .enumerate()
                                .map(|(n, column)| {
                                    let gap = if n + 1 < columns.len() { " " } else { "" };
                                    view! { <span>{format!("{column}{gap}")}</span> }
                                })
                                .collect_view()}
                        </span>
                    }
                    .into_any(),
                    None => view! { <span>{cell.label.clone()}</span> }.into_any(),
                })}
            </td>
        }
        .into_any()
    };
    if split {
        vec![td(true, false), td(false, true)]
    } else {
        vec![td(cell.c0 == 0 && !is_step, false)]
    }
}

/// Every block of the recipe's chart at `factor`.
#[component]
pub fn Chart(
    doc: Arc<RecipeDoc>,
    factor: f64,
    /// Cook-mode state of a cell; without it nothing is marked.
    #[prop(optional)]
    mark: Option<Callback<Cell, CellMark>>,
    on_tap: Callback<Cell>,
    /// Whether ingredient rows react to taps (cook mode crosses them off).
    #[prop(optional)]
    leaf_taps: bool,
) -> impl IntoView {
    let wide = RwSignal::new(viewport_is_wide());
    let resized = window_event_listener(ev::resize, move |_| wide.set(viewport_is_wide()));
    on_cleanup(move || resized.remove());
    chart_blocks(&doc, factor)
        .into_iter()
        .map(|block| {
            // On a wide viewport a block lies flat (every label horizontal)
            // unless that makes it wider than the space it has.
            let flat = RwSignal::new(false);
            let scroller = NodeRef::<html::Div>::new();
            Effect::new(move |_| {
                flat.set(wide.get());
                if wide.get() {
                    request_animation_frame(move || {
                        if let Some(node) = scroller.get_untracked()
                            && node.scroll_width() > node.client_width() + 1
                        {
                            flat.set(false);
                        }
                    });
                }
            });
            let rows = (0..block.rows)
                .map(|row| {
                    let mut cells: Vec<&Cell> =
                        block.cells.iter().filter(|cell| cell.r0 == row).collect();
                    cells.sort_by_key(|cell| cell.c0);
                    let cells: Vec<AnyView> = cells
                        .into_iter()
                        .flat_map(|cell| cell_view(cell, mark, on_tap, leaf_taps))
                        .collect();
                    view! { <tr>{cells}</tr> }
                })
                .collect_view();
            view! {
                <figure class="chart-block" class:flat=move || flat.get()>
                    {(block.title.is_some() || block.note.is_some()).then(|| view! {
                        <figcaption>
                            {block.title.clone()}
                            {block.note.clone().map(|note| view! { <small>{note}</small> })}
                        </figcaption>
                    })}
                    <div class="chart-scroll" node_ref=scroller>
                        <table class="chart"><tbody>{rows}</tbody></table>
                    </div>
                </figure>
            }
        })
        .collect_view()
}
