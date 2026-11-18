//! The chart as an HTML table. The layout is `omni_recipe_format::chart_blocks`;
//! this turns cell rectangles into `rowspan`/`colspan`, sizes the columns for
//! the width at hand and adds taps. All text is horizontal.

use std::sync::Arc;

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use omni_recipe_format::{Cell, CellKind, RecipeDoc, chart_blocks};
use wasm_bindgen::JsCast;

use crate::logic::{ChartLabel, chart_widths, estimated_text_width};
use crate::platform::window;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellMark {
    None,
    Done,
    Ready,
}

/// The width a chart most likely has before it is measured: the page column
/// less its gutters.
fn likely_width() -> f64 {
    let width = window()
        .inner_width()
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(390.0);
    width.min(760.0) - 32.0
}

/// Measures chart text with the font the chart is set in (`.chart` in
/// app.css), so a column is exactly as wide as its longest word needs.
fn text_measure() -> impl Fn(&str, bool) -> f64 {
    const FAMILY: &str = "system-ui, -apple-system, \"Segoe UI\", Roboto, sans-serif";
    let context = crate::platform::document()
        .create_element("canvas")
        .ok()
        .and_then(|canvas| canvas.dyn_into::<web_sys::HtmlCanvasElement>().ok())
        .and_then(|canvas| canvas.get_context("2d").ok().flatten())
        .and_then(|context| context.dyn_into::<web_sys::CanvasRenderingContext2d>().ok());
    move |text, bold| {
        let measured = context.as_ref().and_then(|context| {
            context.set_font(&format!("{} 14px {FAMILY}", if bold { 600 } else { 400 }));
            context
                .measure_text(text)
                .ok()
                .map(|metrics| metrics.width())
        });
        measured
            .filter(|width| *width > 0.0)
            .unwrap_or_else(|| estimated_text_width(text, bold))
    }
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
    let td = |lead: bool, filler: bool| {
        let class = format!(
            "{}{}{}{}",
            cell.kind.as_str(),
            if cell.optional { " optional" } else { "" },
            if lead { " lead" } else { "" },
            if filler { " fill" } else { "" },
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
    let resized = RwSignal::new(0_u32);
    let listener = window_event_listener(ev::resize, move |_| resized.update(|n| *n += 1));
    on_cleanup(move || listener.remove());
    let measure = StoredValue::new_local(text_measure());
    chart_blocks(&doc, factor)
        .into_iter()
        .map(|block| {
            // The block is laid out for the width it really has: measured
            // once it is in the page and again whenever the window changes.
            let avail = RwSignal::new(likely_width());
            let figure = NodeRef::<html::Figure>::new();
            Effect::new(move |_| {
                resized.track();
                if let Some(node) = figure.get() {
                    let width = f64::from(node.client_width());
                    if width > 0.0 && (width - avail.get_untracked()).abs() > 0.5 {
                        avail.set(width);
                    }
                }
            });
            let sized = StoredValue::new((block.cells.clone(), block.cols));
            let widths = Memo::new(move |_| {
                sized.with_value(|(cells, cols)| {
                    let labels: Vec<ChartLabel<'_>> = cells
                        .iter()
                        .map(|cell| ChartLabel {
                            c0: cell.c0,
                            c1: cell.c1,
                            label: &cell.label,
                        })
                        .collect();
                    measure.with_value(|measure| chart_widths(&labels, *cols, avail.get(), measure))
                })
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
                <figure class="chart-block" node_ref=figure>
                    {(block.title.is_some() || block.note.is_some()).then(|| view! {
                        <figcaption>
                            {block.title.clone()}
                            {block.note.clone().map(|note| view! { <small>{note}</small> })}
                        </figcaption>
                    })}
                    <div class="chart-scroll">
                        <table class="chart" style=move || format!("width:{:.1}px", widths.get().iter().sum::<f64>())>
                            <colgroup>
                                {move || widths
                                    .get()
                                    .into_iter()
                                    .map(|width| view! { <col style=format!("width:{width:.1}px")/> })
                                    .collect_view()}
                            </colgroup>
                            <tbody>{rows}</tbody>
                        </table>
                    </div>
                </figure>
            }
        })
        .collect_view()
}
