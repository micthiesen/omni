//! The library: the card grid, search, the tag filter sheet and the
//! "Cooking now" strip.

use leptos::prelude::*;
use omni_api::fond::{ImageVariant, RecipeState, paths};
use omni_recipe_format::format::fmt_time;

use std::collections::BTreeMap;
use std::sync::Arc;

use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use super::requests::{AskSheet, kitchen_items};
use super::{Connection, Link, Sheet, icon, remaining, settled, swallow_next_click};
use crate::logic::library::{
    Sort, column, column_count, filter_groups, moved_by, moved_to, narrowed, pinned, tag_groups,
    visible,
};
use crate::logic::session::{is_open, project};
use crate::logic::{Route, cover, tile_color};
use crate::store::{Ask, Recipe, use_store};

type Recipes = BTreeMap<String, Arc<Recipe>>;

/// The recipes the grid is drawn from (see [`Library`]).
#[derive(Clone, Copy)]
struct Shown(ReadSignal<Recipes>);

/// The pinned card being dragged, whose place in the grid is shown dimmed,
/// and the card it is held over, which is marked as where it will go.
#[derive(Clone, Copy)]
struct Lifted(Memo<Option<(String, Option<String>)>>);

/// Whether two copies of the library would draw the same grid.
fn same_library(a: &Recipes, b: &Recipes) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|((id, a), (other, b))| id == other && (Arc::ptr_eq(a, b) || a.rec == b.rec))
}

/// How long a finger rests on a pinned card before it can be dragged.
const HOLD_MS: u64 = 400;
/// A finger that moves this far before then is scrolling, not holding.
const SLOP_PX: f64 = 10.0;

thread_local! {
    /// A pinned card is being dragged: the page must not scroll under it.
    static DRAGGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static GUARDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keeps the screen from scrolling while a card is dragged. A touch that
/// has not started a scroll yet can still be claimed by cancelling its
/// `touchmove`, which needs a listener that is not passive; one listener
/// for the life of the page does it.
fn guard_drags() {
    if GUARDED.replace(true) {
        return;
    }
    let handler = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
        if DRAGGING.get() && event.cancelable() {
            event.prevent_default();
        }
    });
    let options = web_sys::AddEventListenerOptions::new();
    options.set_passive(false);
    let _ = crate::platform::document()
        .add_event_listener_with_callback_and_add_event_listener_options(
            "touchmove",
            handler.as_ref().unchecked_ref(),
            &options,
        );
    handler.forget();
}

/// A finger down on a pinned card, and from the moment it has rested long
/// enough, the card being dragged.
#[derive(Debug, Clone, PartialEq)]
struct Press {
    id: String,
    /// Where the finger went down, and where it is.
    start: (f64, f64),
    at: (f64, f64),
    /// The finger's offset inside the card, and the card's width.
    grip: (f64, f64),
    width: f64,
    dragging: bool,
    /// The pinned card the finger is over: where the held one will go.
    over: Option<String>,
}

/// The pinned card under a point of the screen.
fn pin_at(x: f64, y: f64) -> Option<String> {
    #[allow(clippy::cast_possible_truncation)]
    let found = crate::platform::document().element_from_point(x as f32, y as f32)?;
    found
        .closest("[data-pin]")
        .ok()
        .flatten()
        .and_then(|card| card.get_attribute("data-pin"))
}

#[component]
fn Card(
    id: String,
    /// Opens the card's menu.
    on_more: Callback<String>,
    /// The card is in the pinned section.
    #[prop(optional)]
    pin: bool,
) -> impl IntoView {
    let store = use_store();
    let Shown(shown) = expect_context::<Shown>();
    let lifted = use_context::<Lifted>();
    move || {
        let recipe = shown.with(|recipes| recipes.get(&id).cloned())?;
        let rec = &recipe.rec;
        let class = if cover(rec).is_some() {
            "card"
        } else {
            "card tiled"
        };
        let art = match cover(rec) {
            Some(image) => view! {
                <div class="cover" style=format!("background-color:{}", image.color)>
                    <img src=paths::image(&image.digest, ImageVariant::Thumb) alt="" loading="lazy" decoding="async" draggable="false"/>
                </div>
            }
            .into_any(),
            None => {
                let (background, text) = store
                    .tags
                    .with(|registry| tile_color(&rec.tags, registry, &recipe.title));
                view! {
                    <div class="cover tile" style=format!("background-color:{background};color:{text}")>
                        <span>{recipe.title.clone()}</span>
                    </div>
                }
                .into_any()
            }
        };
        let time = (rec.total_min > 0).then(|| fmt_time(rec.total_min, None));
        let tags: Vec<String> = rec.tags.iter().take(2).cloned().collect();
        let title = recipe.title.clone();
        let archived = rec.state == RecipeState::Archived;
        let uncooked = rec.cook_count == 0 && rec.state == RecipeState::Active;
        let more = id.clone();
        let label = format!("More for {title}");
        Some(view! {
            <div class="card-wrap" data-card=id.clone() data-pin=pin.then(|| id.clone())
                class:lifted={
                    let id = id.clone();
                    move || pin && lifted.is_some_and(|Lifted(lifted)| {
                        lifted.with(|lifted| lifted.as_ref().is_some_and(|(held, _)| *held == id))
                    })
                }
                class:drop-here={
                    let id = id.clone();
                    move || pin && lifted.is_some_and(|Lifted(lifted)| {
                        lifted.with(|lifted| {
                            lifted.as_ref().is_some_and(|(_, over)| over.as_deref() == Some(id.as_str()))
                        })
                    })
                }>
                <Link to=Route::Recipe(id.clone()) class=class>
                    {art}
                    <h3>{title}</h3>
                    <p class="card-meta">
                        {time.map(|time| view! { <span class="time">{time}</span> })}
                        {tags.into_iter().map(|tag| view! { <span class="tag">{tag}</span> }).collect_view()}
                    </p>
                    {archived.then(|| view! { <span class="badge">"Archived"</span> })}
                    {uncooked.then(|| view! { <span class="badge new">"Not yet cooked"</span> })}
                </Link>
                <button class="card-more" aria-label=label data-testid="card-more"
                    on:click=move |_| on_more.run(more.clone())>{icon("more")}</button>
            </div>
        })
    }
}

#[component]
fn CookingNow() -> impl IntoView {
    let store = use_store();
    let open = move || {
        store.sessions.with(|sessions| {
            let mut open: Vec<_> = sessions.values().filter(|s| is_open(s)).cloned().collect();
            open.sort_by_key(|session| std::cmp::Reverse(session.last_event_at));
            open
        })
    };
    view! {
        <Show when=move || !open().is_empty()>
            <section class="cooking" aria-label="Cooking now">
                <h2>"Cooking now"</h2>
                <div class="cooking-row">
                    {move || {
                        open()
                            .into_iter()
                            .map(|session| {
                                let state = project(&session);
                                let title = store
                                    .recipe(&session.recipe_id)
                                    .map_or_else(|| "Recipe".to_owned(), |recipe| recipe.title.clone());
                                let progress = store.session_doc(&session, &state).map(|(doc, _)| {
                                    let (done, total) = state.progress(&doc);
                                    if done >= total {
                                        format!("all {total} steps done")
                                    } else {
                                        format!("step {} of {total}", done + 1)
                                    }
                                });
                                let timer = state.timers.values().copied().min();
                                view! {
                                    <Link to=Route::Cook(session.id.clone()) class="cooking-card">
                                        <b>{title}</b>
                                        <span>
                                            {progress}
                                            {timer.map(|ends_at| view! {
                                                <span class="cooking-timer">{icon("timer")}{move || remaining(store, ends_at)}</span>
                                            })}
                                        </span>
                                    </Link>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </section>
        </Show>
    }
}

/// The card grid: columns of cards at their own heights, with no row to
/// stretch a short card to. Cards are dealt across the columns in order
/// (`logic::library::column`), so rows still read left to right.
#[component]
fn Masonry(
    ids: Signal<Vec<String>>,
    on_more: Callback<String>,
    testid: &'static str,
    #[prop(optional)] pin: bool,
) -> impl IntoView {
    let store = use_store();
    let columns = Memo::new(move |_| column_count(store.width.get()));
    view! {
        <div class="grid" data-testid=testid>
            {move || {
                let count = columns.get();
                (0..count)
                    .map(|n| view! {
                        <div class="grid-col">
                            <For each=move || ids.with(|ids| column(ids, count, n)) key=|id| id.clone()
                                children=move |id| view! { <Card id=id on_more=on_more pin=pin/> }/>
                        </div>
                    })
                    .collect_view()
            }}
        </div>
    }
}

#[component]
pub fn Library() -> impl IntoView {
    let store = use_store();
    let sheet = RwSignal::new(false);
    let menu = RwSignal::new(false);
    let asking = RwSignal::new(false);
    let card_menu = RwSignal::new(false);
    let card = RwSignal::new(String::new());
    let press = RwSignal::new(None::<Press>);
    // The pinned order while a card is dragged, and until the store has it.
    let order = RwSignal::new(None::<Vec<String>>);
    // A change made from the card menu shows under the menu at once.
    let passing = RwSignal::new(false);
    let dragging =
        Memo::new(move |_| press.with(|press| press.as_ref().is_some_and(|press| press.dragging)));
    provide_context(Lifted(Memo::new(move |_| {
        press.with(|press| {
            press
                .as_ref()
                .filter(|press| press.dragging)
                .map(|press| (press.id.clone(), press.over.clone()))
        })
    })));
    guard_drags();
    // The grid follows the library as it syncs, except under an open sheet:
    // then it keeps what it shows and "Recipes updated · Reload" is offered.
    let shown = settled(
        store,
        "Recipes updated",
        store.recipes.into(),
        same_library,
        Signal::derive(move || {
            sheet.get()
                || menu.get()
                || asking.get()
                || dragging.get()
                || (card_menu.get() && !passing.get())
        }),
    );
    provide_context(Shown(shown));
    let stored_pins = Memo::new(move |_| {
        shown.with(|recipes| pinned(recipes.values().map(|recipe| &recipe.item)))
    });
    let pins = Memo::new(move |_| order.get().unwrap_or_else(|| stored_pins.get()));
    // Once the store shows the order a drag or a move made, it is the order.
    Effect::new(move |_| {
        let stored = stored_pins.get();
        if order.with_untracked(|order| order.as_ref() == Some(&stored)) {
            order.set(None);
        }
    });
    // Pinned recipes have their own section over the whole library; a search
    // or a filter lists every match in one grid instead.
    let whole = Memo::new(move |_| !store.filter.with(narrowed));
    let set_pins = move |ids: Vec<String>| {
        if ids == stored_pins.get_untracked() {
            order.set(None);
            return;
        }
        order.set(Some(ids.clone()));
        passing.set(true);
        store.set_pins(ids);
        request_animation_frame(move || passing.set(false));
    };
    let on_more = Callback::new(move |id: String| {
        card.set(id);
        card_menu.set(true);
    });
    let hold = StoredValue::new(None::<TimeoutHandle>);
    let release = move || {
        if let Some(timer) = hold.get_value() {
            timer.clear();
            hold.set_value(None);
        }
        DRAGGING.set(false);
        press.set(None);
    };
    on_cleanup(move || {
        if let Some(timer) = hold.try_get_value().flatten() {
            timer.clear();
        }
        DRAGGING.set(false);
    });
    let down = move |event: web_sys::PointerEvent| {
        if !event.is_primary() || event.button() != 0 {
            return;
        }
        let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        // The menu button is a button, not a handle.
        if target.closest(".card-more").ok().flatten().is_some() {
            return;
        }
        let Some(wrap) = target.closest("[data-pin]").ok().flatten() else {
            return;
        };
        let Some(id) = wrap.get_attribute("data-pin") else {
            return;
        };
        let rect = wrap.get_bounding_client_rect();
        let at = (f64::from(event.client_x()), f64::from(event.client_y()));
        release();
        press.set(Some(Press {
            id,
            start: at,
            at,
            grip: (at.0 - rect.left(), at.1 - rect.top()),
            width: rect.width(),
            dragging: false,
            over: None,
        }));
        let pointer = event.pointer_id();
        let section = event
            .current_target()
            .and_then(|section| section.dyn_into::<web_sys::Element>().ok());
        let timer = set_timeout_with_handle(
            move || {
                hold.set_value(None);
                if press.try_with_untracked(Option::is_some) == Some(true) {
                    DRAGGING.set(true);
                    // A mouse released outside the section still ends the
                    // drag here (a finger is captured by what it touched).
                    if let Some(section) = &section {
                        let _ = section.set_pointer_capture(pointer);
                    }
                    press.update(|press| {
                        if let Some(press) = press {
                            press.dragging = true;
                        }
                    });
                }
            },
            std::time::Duration::from_millis(HOLD_MS),
        );
        hold.set_value(timer.ok());
    };
    let moving = move |event: web_sys::PointerEvent| {
        let Some(held) = press.get_untracked() else {
            return;
        };
        let at = (f64::from(event.client_x()), f64::from(event.client_y()));
        if !held.dragging {
            // The finger moved before the hold was up: it is a scroll or a swipe.
            if (at.0 - held.start.0).hypot(at.1 - held.start.1) > SLOP_PX {
                release();
            }
            return;
        }
        event.prevent_default();
        // The cards stay where they are while one is held: the card under
        // the finger is only marked as the place the held one will take.
        // Moving them now would take the pressed element out of the page,
        // and with it the touch that is dragging.
        let over = pin_at(at.0, at.1).filter(|over| *over != held.id);
        press.update(|press| {
            if let Some(press) = press {
                press.at = at;
                press.over = over;
            }
        });
    };
    let up = move |_| {
        let held = press.get_untracked().filter(|press| press.dragging);
        release();
        if let Some(held) = held {
            // The browser follows the lift with a click on the card.
            swallow_next_click();
            let list = pins.get_untracked();
            if let Some(to) = held
                .over
                .and_then(|over| list.iter().position(|id| *id == over))
            {
                set_pins(moved_to(&list, &held.id, to));
            }
        }
    };
    let cancel = move |_| release();
    let ids = Memo::new(move |_| {
        let groups = store.tags.with(|tags| tag_groups(tags));
        let mut found = shown.with(|recipes| {
            visible(
                recipes.values().map(|recipe| &recipe.item),
                &store.filter.get(),
                &groups,
                store.sort.get(),
            )
        });
        if whole.get() {
            let above = pins.get();
            found.retain(|id| !above.contains(id));
        }
        found
    });
    let groups = move || {
        shown.with(|recipes| {
            store.tags.with(|registry| {
                filter_groups(
                    recipes.values().map(|recipe| &recipe.item),
                    registry,
                    &store.filter.get(),
                )
            })
        })
    };
    let active_filters = move || {
        store
            .filter
            .with(|filter| filter.tags.len() + usize::from(filter.archived))
    };
    let scroll = store.library_scroll.get_untracked();
    if scroll > 0.0 {
        request_animation_frame(move || crate::platform::shell::set_scroll_top(scroll));
    }

    view! {
        <main class="page library">
            <header class="bar">
                <h1 class="wordmark">"Fond"</h1>
                <Connection/>
                <Link to=Route::Inbox class="icon-btn" label="Inbox">
                    {icon("camera")}
                    {move || {
                        let pending = store.inbox_pending.get();
                        (pending > 0).then(|| view! { <span class="count">{pending}</span> })
                    }}
                </Link>
                <button class="icon-btn" aria-label="More" on:click=move |_| menu.set(true)>{icon("more")}</button>
            </header>
            <div class="scroller" data-testid="scroller"><div class="scroller-in">
            <div class="tools">
                <input
                    type="search"
                    class="search"
                    placeholder="Search recipes"
                    aria-label="Search recipes"
                    autocomplete="off"
                    autocapitalize="off"
                    enterkeyhint="search"
                    prop:value=move || store.filter.with(|filter| filter.query.clone())
                    on:input=move |event| {
                        let query = event_target_value(&event);
                        store.filter.update(|filter| filter.query = query);
                    }
                />
                <button class="tool" class:on={move || active_filters() > 0} aria-label="Filter"
                    on:click=move |_| sheet.set(true)>
                    {icon("filter")}
                    {move || (active_filters() > 0).then(|| view! { <span class="count">{active_filters()}</span> })}
                </button>
                <select class="tool sort" aria-label="Sort"
                    on:change=move |event| store.set_sort(Sort::parse(&event_target_value(&event)))>
                    {Sort::ALL
                        .into_iter()
                        .map(|sort| view! {
                            <option value=sort.as_str() selected=move || store.sort.get() == sort>{sort.label()}</option>
                        })
                        .collect_view()}
                </select>
            </div>
            <CookingNow/>
            <Show when=move || whole.get() && pins.with(|pins| !pins.is_empty())>
                <section class="pinned" class:dragging=move || dragging.get() aria-label="Pinned"
                    on:pointerdown=down on:pointermove=moving on:pointerup=up on:pointercancel=cancel
                    on:lostpointercapture=move |_| {
                        // A capture that ends without a lift (the window lost the pointer).
                        if dragging.get_untracked() {
                            release();
                        }
                    }
                    on:contextmenu=move |event: web_sys::MouseEvent| event.prevent_default()>
                    <h2>"Pinned"</h2>
                    <Masonry ids=pins.into() on_more=on_more testid="pinned" pin=true/>
                </section>
            </Show>
            <Masonry ids=ids.into() on_more=on_more testid="grid"/>
            <Show when=move || ids.with(Vec::is_empty) && !(whole.get() && pins.with(|pins| !pins.is_empty()))>
                <p class="empty">
                    {move || {
                        if shown.with(BTreeMap::is_empty) {
                            if store.has_library.get() {
                                "No recipes yet. Ask an agent to add one, or send a photo from the Inbox."
                            } else if store.online.get() {
                                "Loading your recipes…"
                            } else {
                                "Open Fond once on your home network to load your recipes."
                            }
                        } else {
                            "No recipes match."
                        }
                    }}
                </p>
            </Show>
            </div></div>
        </main>
        // The card being dragged, under the finger; its place in the grid
        // is held by the dimmed card.
        {move || {
            press.get().filter(|press| press.dragging).map(|press| {
                let style = format!(
                    "left:{}px;top:{}px;width:{}px",
                    press.at.0 - press.grip.0,
                    press.at.1 - press.grip.1,
                    press.width
                );
                view! {
                    <div class="drag-ghost" style=style aria-hidden="true" data-testid="drag-ghost">
                        <Card id=press.id.clone() on_more=Callback::new(|_| {})/>
                    </div>
                }
            })
        }}
        <Sheet open=card_menu title=Signal::derive(move || {
            store.recipe(&card.get()).map(|recipe| recipe.title.clone()).unwrap_or_default()
        })>
            <div class="menu">
                {move || {
                    let id = card.get();
                    let list = pins.get();
                    let place = list.iter().position(|pin| *pin == id);
                    let last = list.len().saturating_sub(1);
                    let (pin_id, up_id, down_id) = (id.clone(), id.clone(), id);
                    let (up_list, down_list) = (list.clone(), list);
                    view! {
                        <button data-testid="card-pin" on:click=move |_| {
                            card_menu.set(false);
                            store.pin(&pin_id, place.is_none());
                            store.show_toast(
                                if place.is_some() { "Unpinned" } else { "Pinned to the top of the library" },
                                None,
                            );
                        }>{if place.is_some() { "Unpin" } else { "Pin" }}</button>
                        {place.map(|place| view! {
                            <button data-testid="card-up" disabled=place == 0
                                on:click=move |_| set_pins(moved_by(&up_list, &up_id, -1))>
                                "Move up"
                                <small>{format!("Pinned, {} of {}", place + 1, last + 1)}</small>
                            </button>
                            <button data-testid="card-down" disabled=place == last
                                on:click=move |_| set_pins(moved_by(&down_list, &down_id, 1))>
                                "Move down"
                            </button>
                        })}
                    }
                }}
            </div>
        </Sheet>
        <Sheet open=menu title="Library">
            <div class="menu">{kitchen_items(menu, asking)}</div>
        </Sheet>
        <AskSheet open=asking about=Callback::new(|()| Ask::library())/>
        <Sheet open=sheet title="Filter">
            {move || {
                groups()
                    .into_iter()
                    .map(|group| view! {
                        <h3 class="group">{group.label}</h3>
                        <div class="chips">
                            {group
                                .tags
                                .into_iter()
                                .map(|(tag, count)| {
                                    let name = tag.clone();
                                    let on = store.filter.with(|filter| filter.tags.contains(&tag));
                                    view! {
                                        <button class="chip" class:on=on aria-pressed=on.to_string()
                                            on:click=move |_| store.filter.update(|filter| {
                                                if !filter.tags.remove(&name) {
                                                    filter.tags.insert(name.clone());
                                                }
                                            })>
                                            {tag}<small>{count}</small>
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                    })
                    .collect_view()
            }}
            <label class="switch-row">
                <span>"Archived"</span>
                <input type="checkbox" role="switch"
                    prop:checked=move || store.filter.with(|filter| filter.archived)
                    on:change=move |event| {
                        let on = event_target_checked(&event);
                        store.filter.update(|filter| filter.archived = on);
                    }/>
            </label>
            <div class="sheet-actions">
                <button class="btn ghost" on:click=move |_| store.filter.update(|filter| {
                    filter.tags.clear();
                    filter.archived = false;
                })>"Clear"</button>
                <button class="btn" on:click=move |_| sheet.set(false)>
                    {move || format!("Show {} recipes", ids.with(Vec::len))}
                </button>
            </div>
        </Sheet>
    }
}
