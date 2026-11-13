//! The application shell: rail, top bar, phone tab bar, command palette,
//! toast region, shortcut sheet and global keyboard shortcuts.

mod bars;
mod connection;
pub mod nav;
mod palette;
mod rail;

use std::cell::Cell;
use std::rc::Rc;

use leptos::prelude::*;
use omni_api::streamers::StreamerView;
use omni_web_kit::chrome::{ChromeContext, provide_chrome};
use omni_web_kit::components::{Inspector, ToastRegion, provide_toasts};
use omni_web_kit::hooks::{store_pref, stored_pref, use_now};
use omni_web_kit::live::use_live_data;
use omni_web_kit::router::navigate;
use omni_web_kit::task::on_cleanup_local;
use omni_web_kit::utils::js::now_ms;
use omni_web_kit::utils::tasks::task_health;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;

use self::bars::{TabBar, TopBar};
use self::nav::{CrumbNames, crumbs, current_href, shortcut_target};
use self::palette::Palette;
use self::rail::Rail;
use crate::routes::Route;

const RAIL_PREF: &str = "omni.rail";
/// How long a `g` prefix waits for its second key.
const G_WINDOW_MS: f64 = 1500.0;

/// Shell state shared with its parts.
#[derive(Clone, Copy)]
pub struct ShellContext {
    pub palette_open: RwSignal<bool>,
    pub help_open: RwSignal<bool>,
    pub rail_collapsed: RwSignal<bool>,
}

impl ShellContext {
    pub fn toggle_rail(self) {
        let next = !self.rail_collapsed.get_untracked();
        self.rail_collapsed.set(next);
        store_pref(RAIL_PREF, if next { "collapsed" } else { "expanded" });
    }
}

/// Keystrokes in text entry never trigger shortcuts.
fn typing_target(event: &web_sys::KeyboardEvent) -> bool {
    let Some(el) = event
        .target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return false;
    };
    matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") || el.is_content_editable()
}

/// Moves focus through the page's primary rows (`[data-row]` inside
/// `[data-primary-rows]`).
fn step_rows(forward: bool) {
    let Ok(rows) = document().query_selector_all("[data-primary-rows] [data-row]") else {
        return;
    };
    let rows: Vec<web_sys::HtmlElement> = (0..rows.length())
        .filter_map(|i| rows.get(i))
        .filter_map(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
        .collect();
    if rows.is_empty() {
        return;
    }
    let active = document().active_element();
    let index = rows.iter().position(|row| {
        active
            .as_ref()
            .is_some_and(|a| row.is_same_node(Some(a.unchecked_ref())))
    });
    let next = match (index, forward) {
        (None, _) => 0,
        (Some(i), true) => (i + 1).min(rows.len() - 1),
        (Some(i), false) => i.saturating_sub(1),
    };
    let row = &rows[next];
    let _ = row.focus();
    let options = web_sys::ScrollIntoViewOptions::new();
    options.set_block(web_sys::ScrollLogicalPosition::Nearest);
    row.scroll_into_view_with_scroll_into_view_options(&options);
}

fn install_shortcuts(shell: ShellContext) {
    let pending_g = Rc::new(Cell::new(0.0_f64));
    let handler =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
            let key = event.key();
            if (event.meta_key() || event.ctrl_key()) && key.eq_ignore_ascii_case("k") {
                event.prevent_default();
                shell.palette_open.update(|open| *open = !*open);
                return;
            }
            if event.default_prevented()
                || event.meta_key()
                || event.ctrl_key()
                || event.alt_key()
                || typing_target(&event)
                || shell.palette_open.get_untracked()
                || shell.help_open.get_untracked()
                || document()
                    .query_selector("[aria-modal=\"true\"]")
                    .ok()
                    .flatten()
                    .is_some()
            {
                return;
            }
            let g_armed = now_ms() - pending_g.get() < G_WINDOW_MS;
            pending_g.set(0.0);
            if g_armed
                && let Some(target) = key
                    .chars()
                    .next()
                    .filter(|_| key.len() == 1)
                    .and_then(shortcut_target)
            {
                event.prevent_default();
                navigate(target);
                return;
            }
            match key.as_str() {
                "g" => pending_g.set(now_ms()),
                "/" => {
                    event.prevent_default();
                    let filter = document()
                        .query_selector("[data-page-filter]")
                        .ok()
                        .flatten()
                        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
                    match filter {
                        Some(filter) => {
                            let _ = filter.focus();
                        }
                        None => shell.palette_open.set(true),
                    }
                }
                "[" => shell.toggle_rail(),
                "?" => shell.help_open.set(true),
                "j" => step_rows(true),
                "k" => step_rows(false),
                _ => {}
            }
        });
    let doc = document();
    let _ = doc.add_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
    on_cleanup_local(move || {
        let _ =
            doc.remove_event_listener_with_callback("keydown", handler.as_ref().unchecked_ref());
    });
}

const SHORTCUTS: [(&str, &str); 8] = [
    ("⌘K  /", "Search and go anywhere"),
    (
        "g then h l m p e a o c d",
        "Home, Live, Media, Podcasts, Email, Calendar, Operations, Costs, Data",
    ),
    ("j  k", "Move through the page's rows"),
    ("Enter", "Open or inspect the focused row"),
    ("[", "Collapse or expand the sidebar"),
    ("Esc", "Close the palette, sheet, inspector or modal"),
    ("?", "This sheet"),
    ("/", "Focus the page filter when there is one"),
];

#[component]
fn ShortcutSheet(on_close: Callback<()>) -> impl IntoView {
    view! {
        <Inspector title="Keyboard shortcuts" on_close>
            <section class="inspector-section">
                <dl class="kv shortcuts">
                    {SHORTCUTS
                        .iter()
                        .map(|(keys, what)| view! {
                            <dt>
                                {keys.split("  ").map(|k| view! { <kbd class="kbd">{k.to_owned()}</kbd> " " }).collect_view()}
                            </dt>
                            <dd>{*what}</dd>
                        })
                        .collect_view()}
                </dl>
            </section>
        </Inspector>
    }
}

/// Wraps the routed page in the shell. `route` and `path` are normalized.
#[component]
pub fn Shell(route: Memo<Route>, path: Memo<String>, children: ChildrenFn) -> impl IntoView {
    let shell = ShellContext {
        palette_open: RwSignal::new(false),
        help_open: RwSignal::new(false),
        rail_collapsed: RwSignal::new(stored_pref(RAIL_PREF).as_deref() == Some("collapsed")),
    };
    provide_context(shell);
    provide_toasts();
    let chrome: ChromeContext = provide_chrome();
    let live = use_live_data();
    install_shortcuts(shell);

    let current = Signal::derive(move || path.with(|p| current_href(p)));
    let names = Memo::new(move |_| {
        let route = route.get();
        let page = path.with(|p| chrome.label_for(p));
        let streamer = match &route {
            Route::Streamer(id) | Route::StreamerIntelligence(id) => live.snapshot.with(|s| {
                s.as_ref().and_then(|s| {
                    s.streamers.iter().find(|v| v.id() == id).map(|v| match v {
                        StreamerView::Live(l) => l.display_name.clone(),
                        StreamerView::Offline(o) => o.display_name.clone(),
                    })
                })
            }),
            _ => None,
        };
        CrumbNames { page, streamer }
    });
    let trail = Signal::derive(move || {
        let route = route.get();
        let streamer_id = match &route {
            Route::Streamer(id) | Route::StreamerIntelligence(id) => Some(id.as_str()),
            _ => None,
        };
        path.with(|p| crumbs(p, streamer_id, &names.get()))
    });

    let now = use_now(30_000);
    let live_count = Signal::derive(move || {
        live.snapshot.with(|s| {
            s.as_ref()
                .map_or(0, |s| s.streamers.iter().filter(|v| v.is_live()).count())
        })
    });
    let faults = Signal::derive(move || {
        let now = now.get();
        live.snapshot.with(|s| {
            s.as_ref().map_or(0, |s| {
                s.tasks
                    .iter()
                    .filter(|t| task_health(t, now).needs_attention())
                    .count()
            })
        })
    });

    let focus_mode = move || path.with(|p| p.starts_with("/feedback/"));
    let content_class = move || {
        if focus_mode() {
            "content focus"
        } else if path.with(|p| matches!(p.as_str(), "/data" | "/mcp-activity" | "/operations")) {
            "content full"
        } else {
            "content"
        }
    };

    view! {
        <div
            class=move || if focus_mode() { "app focus-mode" } else { "app" }
            data-rail=move || if shell.rail_collapsed.get() { "collapsed" } else { "expanded" }
        >
            <a href="#main-content" class="skip-link">"Skip to content"</a>
            <Rail shell path=Signal::derive(move || path.get()) current/>
            <div class="main">
                <TopBar shell crumbs=trail/>
                <main id="main-content" tabindex="-1" class=content_class>
                    {children()}
                </main>
            </div>
            <TabBar shell path=Signal::derive(move || path.get()) live_count faults/>
            {move || shell.palette_open.get().then(|| view! {
                <Palette on_close=Callback::new(move |()| shell.palette_open.set(false))/>
            })}
            {move || shell.help_open.get().then(|| view! {
                <ShortcutSheet on_close=Callback::new(move |()| shell.help_open.set(false))/>
            })}
            <ToastRegion/>
        </div>
    }
}
