//! The chart as an HTML table. The layout is `omni_recipe_format::chart_blocks`;
//! this only turns cell rectangles into `rowspan`/`colspan` and adds taps.

use std::sync::Arc;

use leptos::prelude::*;
use omni_recipe_format::{Cell, CellKind, RecipeDoc, chart_blocks};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellMark {
    None,
    Done,
    Ready,
}

/// Step cells spanning at least this many rows set their text vertically, as
/// the original notation does, which keeps wide charts narrow.
const VERTICAL_ROWS: usize = 3;

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
    let td = |lead: bool, filler: bool| {
        let class = format!(
            "{}{}{}{}{}",
            cell.kind.as_str(),
            if cell.optional { " optional" } else { "" },
            if lead { " lead" } else { "" },
            if filler { " fill" } else { "" },
            if is_step && rows >= VERTICAL_ROWS {
                " vertical"
            } else {
                ""
            },
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
                {(!filler).then(|| view! { <span>{cell.label.clone()}</span> })}
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
    chart_blocks(&doc, factor)
        .into_iter()
        .map(|block| {
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
                <figure class="chart-block">
                    {block.title.clone().map(|title| view! {
                        <figcaption>
                            {title}
                            {block.note.clone().map(|note| view! { <small>{note}</small> })}
                        </figcaption>
                    })}
                    <div class="chart-scroll">
                        <table class="chart"><tbody>{rows}</tbody></table>
                    </div>
                </figure>
            }
        })
        .collect_view()
}
