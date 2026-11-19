//! The library: the card grid, search, the tag filter sheet and the
//! "Cooking now" strip.

use leptos::prelude::*;
use omni_api::fond::{ImageVariant, RecipeState, paths};
use omni_recipe_format::format::fmt_time;

use super::{Connection, Link, Sheet, icon, remaining};
use crate::logic::library::{Sort, filter_groups, tag_groups, visible};
use crate::logic::session::{is_open, project};
use crate::logic::{Route, cover, tile_color};
use crate::store::use_store;

#[component]
fn Card(id: String) -> impl IntoView {
    let store = use_store();
    move || {
        let recipe = store.recipe(&id)?;
        let rec = &recipe.rec;
        let class = if cover(rec).is_some() {
            "card"
        } else {
            "card tiled"
        };
        let art = match cover(rec) {
            Some(image) => view! {
                <div class="cover" style=format!("background-color:{}", image.color)>
                    <img src=paths::image(&image.digest, ImageVariant::Thumb) alt="" loading="lazy" decoding="async"/>
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
        Some(view! {
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

#[component]
pub fn Library() -> impl IntoView {
    let store = use_store();
    let sheet = RwSignal::new(false);
    let ids = Memo::new(move |_| {
        let groups = store.tags.with(|tags| tag_groups(tags));
        store.recipes.with(|recipes| {
            visible(
                recipes.values().map(|recipe| &recipe.item),
                &store.filter.get(),
                &groups,
                store.sort.get(),
            )
        })
    });
    let groups = move || {
        store.recipes.with(|recipes| {
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
            <div class="grid" data-testid="grid">
                <For each=move || ids.get() key=|id| id.clone() children=move |id| view! { <Card id=id/> }/>
            </div>
            <Show when=move || ids.with(Vec::is_empty)>
                <p class="empty">
                    {move || {
                        if store.recipes.with(|recipes| recipes.is_empty()) {
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
