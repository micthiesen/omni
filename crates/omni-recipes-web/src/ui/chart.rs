//! The chart as an HTML table. The layout is `omni_recipe_format::chart_blocks`;
//! this turns cell rectangles into `rowspan`/`colspan`, sizes the columns for
//! the width at hand and adds taps. All text is horizontal. Cook mode adds a
//! press and hold on step cells and a running timer's countdown
//! (docs/fond/cook-chart-steps.md).

use std::sync::Arc;
use std::time::Duration;

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use omni_api::common::Ms;
use omni_recipe_format::{Cell, CellKind, RecipeDoc, chart_blocks};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use super::{icon, remaining};
use crate::logic::{ChartLabel, chart_widths, estimated_text_width};
use crate::platform::{document, now_ms, window};
use crate::store::{Store, use_store};

/// How long a press on a step cell lasts before it counts as a hold.
const HOLD_MS: u64 = 450;
/// A press that travels further than this is a scroll, not a hold.
const HOLD_SLOP_PX: i32 = 10;
/// A press that began this soon after the page scrolled, or during which it
/// scrolled, is a finger stopping or moving the page.
const SCROLL_QUIET_MS: Ms = 300;
/// How long a cell shows that its hold was taken.
const HELD_FLASH_MS: u64 = 400;

thread_local! {
    static LAST_SCROLL: std::cell::Cell<Ms> = const { std::cell::Cell::new(0) };
    static WATCHING_SCROLLS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Notes when anything on the page scrolls. Scroll events do not bubble, so
/// one capturing listener on the document sees every scroller's.
fn watch_scrolls() {
    if WATCHING_SCROLLS.replace(true) {
        return;
    }
    let noted = Closure::<dyn FnMut(web_sys::Event)>::new(|_| LAST_SCROLL.set(now_ms()));
    let _ = document().add_event_listener_with_callback_and_bool(
        "scroll",
        noted.as_ref().unchecked_ref(),
        true,
    );
    noted.forget();
}

/// Press and hold on one step cell. A set-up cell drawn as two table cells
/// shares one of these between its halves.
#[derive(Clone, Copy)]
struct Hold {
    on_hold: Callback<Cell>,
    /// The press in progress: its timer and where it began.
    pending: StoredValue<Option<(TimeoutHandle, i32, i32)>>,
    /// The press became a hold, so the click that ends it is not a tap.
    held: StoredValue<bool>,
    flash: RwSignal<bool>,
}

impl Hold {
    fn new(on_hold: Callback<Cell>) -> Self {
        watch_scrolls();
        let hold = Self {
            on_hold,
            pending: StoredValue::new(None),
            held: StoredValue::new(false),
            flash: RwSignal::new(false),
        };
        on_cleanup(move || hold.cancel());
        hold
    }

    fn cancel(self) {
        if let Some((timer, ..)) = self.pending.try_update_value(Option::take).flatten() {
            timer.clear();
        }
    }

    fn begin(self, event: &web_sys::PointerEvent, cell: Cell) {
        self.cancel();
        self.held.set_value(false);
        if !event.is_primary() || event.button() != 0 {
            return;
        }
        let down = now_ms();
        // The chart can leave the page while the timer runs (a tab change, a
        // rebase), so nothing here may assume the cell still exists.
        let fire = move || {
            if self.pending.try_set_value(None).is_some()
                || LAST_SCROLL.get() >= down - SCROLL_QUIET_MS
            {
                return;
            }
            self.held.set_value(true);
            self.flash.set(true);
            set_timeout(
                move || {
                    self.flash.try_set(false);
                },
                Duration::from_millis(HELD_FLASH_MS),
            );
            self.on_hold.try_run(cell);
        };
        if let Ok(timer) = set_timeout_with_handle(fire, Duration::from_millis(HOLD_MS)) {
            self.pending
                .set_value(Some((timer, event.client_x(), event.client_y())));
        }
    }

    fn moved(self, event: &web_sys::PointerEvent) {
        if let Some((_, x, y)) = self.pending.get_value()
            && ((event.client_x() - x).abs() > HOLD_SLOP_PX
                || (event.client_y() - y).abs() > HOLD_SLOP_PX)
        {
            self.cancel();
        }
    }

    /// The pointer lifted or left the cell. A touch leaves as it lifts, and
    /// its click comes after both, so leaving must not forget the hold. A
    /// click follows at once when the browser sends one; a hold that gets
    /// none (iOS may send none after a long press) must not swallow a later
    /// click that has no press of its own, such as one from a screen reader.
    fn lifted(self) {
        self.cancel();
        if self.held.get_value() {
            set_timeout(
                move || {
                    self.held.try_set_value(false);
                },
                Duration::from_millis(HELD_FLASH_MS),
            );
        }
    }

    /// The browser took the touch (for a scroll): no click follows.
    fn dropped(self) {
        self.cancel();
        self.held.set_value(false);
    }

    /// Whether the click now arriving ends a hold.
    fn take_held(self) -> bool {
        self.held.try_update_value(std::mem::take).unwrap_or(false)
    }
}

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

/// Whole-pixel columns: a fraction of a pixel puts neighbouring lines on
/// different device pixels, so some look thinner than others or vanish. The
/// ingredient column takes the remainder, so the total is unchanged.
fn whole_pixels(mut widths: Vec<f64>) -> Vec<f64> {
    let total = widths.iter().sum::<f64>().round();
    for width in widths.iter_mut().skip(1) {
        *width = width.round();
    }
    let rest: f64 = widths.iter().skip(1).sum();
    if let Some(lead) = widths.first_mut() {
        *lead = total - rest;
    }
    widths
}

/// The rows whose input goes into the same step as the input on the next
/// row: the line between two such rows is drawn lighter than a line that
/// ends a step (docs/fond/chart-borders.md).
fn same_step_rows(cells: &[Cell], rows: usize) -> Vec<bool> {
    let mut step_of: Vec<Option<&str>> = vec![None; rows];
    for cell in cells {
        if cell.c0 == 0 && matches!(cell.kind, CellKind::Leaf | CellKind::Ref) && cell.r0 < rows {
            step_of[cell.r0] = Some(cell.step.as_str());
        }
    }
    (0..rows)
        .map(|row| step_of[row].is_some() && step_of.get(row + 1) == Some(&step_of[row]))
        .collect()
}

fn span(count: usize) -> Option<String> {
    (count > 1).then(|| count.to_string())
}

/// What cook mode adds to a chart; the recipe page passes none of it.
#[derive(Clone, Copy)]
struct Live {
    store: Store,
    mark: Option<Callback<Cell, CellMark>>,
    on_hold: Option<Callback<Cell>>,
    timer: Option<Callback<Cell, Option<Ms>>>,
}

fn cell_view(
    cell: &Cell,
    live: Live,
    on_tap: Callback<Cell>,
    leaf_taps: bool,
    same_step: bool,
) -> Vec<AnyView> {
    let Live { store, mark, .. } = live;
    let rows = cell.r1 - cell.r0 + 1;
    let cols = cell.c1 - cell.c0 + 1;
    let is_step = cell.kind == CellKind::Step;
    let does_work = matches!(cell.kind, CellKind::Step | CellKind::Setup);
    let tappable = leaf_taps || does_work;
    let hold = live.on_hold.filter(|_| does_work).map(Hold::new);
    let timer = live.timer.filter(|_| does_work);
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
            if same_step { " same-step" } else { "" },
        );
        let (marked, tapped, keyed) = (cell.clone(), cell.clone(), cell.clone());
        let (pressed, timed) = (cell.clone(), cell.clone());
        let state = move || mark.map_or(CellMark::None, |mark| mark.run(marked.clone()));
        let (done, ready, crossed) = (state.clone(), state.clone(), state);
        // The empty stretch of a split cell has no name to be a button by;
        // its labelled half is the one a keyboard or a screen reader reaches.
        let button = tappable && !filler;
        // Only an input is pressed or not; a step cell opens its sheet.
        let pressed_state = (button && !does_work && mark.is_some())
            .then_some(move || (crossed() == CellMark::Done).to_string());
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
                class:held=move || hold.is_some_and(|hold| hold.flash.get())
                data-key=cell.key.clone()
                rowspan=span(rows)
                colspan=colspan
                role=button.then_some("button")
                tabindex=button.then_some("0")
                aria-pressed=pressed_state
                on:click=move |_| {
                    if tappable && !hold.is_some_and(Hold::take_held) {
                        on_tap.run(tapped.clone());
                    }
                }
                on:keydown=move |event| {
                    if button && matches!(event.key().as_str(), "Enter" | " ") {
                        event.prevent_default();
                        on_tap.run(keyed.clone());
                    }
                }
                on:pointerdown=move |event| {
                    if let Some(hold) = hold {
                        hold.begin(&event, pressed.clone());
                    }
                }
                on:pointermove=move |event| {
                    if let Some(hold) = hold {
                        hold.moved(&event);
                    }
                }
                on:pointerup=move |_| {
                    if let Some(hold) = hold {
                        hold.lifted();
                    }
                }
                on:pointercancel=move |_| {
                    if let Some(hold) = hold {
                        hold.dropped();
                    }
                }
                on:pointerleave=move |_| {
                    if let Some(hold) = hold {
                        hold.lifted();
                    }
                }
                on:contextmenu=move |event| {
                    // A long press must not also open a menu.
                    if hold.is_some() {
                        event.prevent_default();
                    }
                }
            >
                {(!filler).then(|| view! { <span>{cell.label.clone()}</span> })}
                {timer.filter(|_| !filler).map(|timer| move || {
                    timer.run(timed.clone()).map(|ends_at| view! {
                        <span class="cell-timer" data-testid="cell-timer" class:late=move || ends_at <= store.now.get()>
                            {icon("timer")}
                            <b>{move || remaining(store, ends_at)}</b>
                        </span>
                    })
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
    /// Press and hold on a step cell (cook mode marks the step done).
    #[prop(optional)]
    on_hold: Option<Callback<Cell>>,
    /// When the timer running on a step cell ends; the cell counts down to it.
    #[prop(optional)]
    timer: Option<Callback<Cell, Option<Ms>>>,
) -> impl IntoView {
    let live = Live {
        store: use_store(),
        mark,
        on_hold,
        timer,
    };
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
                    measure.with_value(|measure| {
                        whole_pixels(chart_widths(&labels, *cols, avail.get(), measure))
                    })
                })
            });
            let same_step = same_step_rows(&block.cells, block.rows);
            // Scrolled sideways, the pinned ingredient column is a pane with
            // an edge of its own over whatever passes under it.
            let scrolled = RwSignal::new(false);
            let scroller = NodeRef::<html::Div>::new();
            let on_scroll = move |_| {
                if let Some(node) = scroller.get_untracked() {
                    let now = node.scroll_left() > 0;
                    if scrolled.get_untracked() != now {
                        scrolled.set(now);
                    }
                }
            };
            let rows = (0..block.rows)
                .map(|row| {
                    let mut cells: Vec<&Cell> =
                        block.cells.iter().filter(|cell| cell.r0 == row).collect();
                    cells.sort_by_key(|cell| cell.c0);
                    let cells: Vec<AnyView> = cells
                        .into_iter()
                        .flat_map(|cell| {
                            let same_step = cell.c0 == 0 && same_step[row];
                            cell_view(cell, live, on_tap, leaf_taps, same_step)
                        })
                        .collect();
                    view! { <tr>{cells}</tr> }
                })
                .collect_view();
            view! {
                <figure class="chart-block" class:scrolled=move || scrolled.get() node_ref=figure>
                    {(block.title.is_some() || block.note.is_some()).then(|| view! {
                        <figcaption>
                            {block.title.clone()}
                            {block.note.clone().map(|note| view! { <small>{note}</small> })}
                        </figcaption>
                    })}
                    <div class="chart-frame">
                        <div class="chart-scroll" node_ref=scroller on:scroll=on_scroll>
                            <table class="chart" style=move || format!("width:{:.0}px", widths.get().iter().sum::<f64>())>
                                <colgroup>
                                    {move || widths
                                        .get()
                                        .into_iter()
                                        .map(|width| view! { <col style=format!("width:{width:.0}px")/> })
                                        .collect_view()}
                                </colgroup>
                                <tbody>{rows}</tbody>
                            </table>
                        </div>
                        <div
                            class="chart-edge"
                            style=move || format!("left:{:.0}px", widths.get().first().copied().unwrap_or_default() - 1.0)
                        ></div>
                    </div>
                </figure>
            }
        })
        .collect_view()
}
