//! The recipe page: both renderings at the chosen scale, notes, the stored
//! original, archive and delete.

use std::sync::Arc;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use omni_api::fond::{Adjustment, ImageVariant, Original, RecipeRecord, RecipeState, paths};
use omni_recipe_format::{Cell, Index, Kind, RecipeDoc, Segment, equipment, list_view};
use wasm_bindgen::JsCast;

use super::chart::Chart;
use super::{Connection, Link, Sheet, back, icon, navigate, replace};
use crate::logic::session::is_open;
use crate::logic::{Route, SCALES, gallery, same_scale, step_servings, tile_color};
use crate::store::{Recipe, Store, Undo, use_store};

/// Step text with ingredient amounts, times and temperatures in bold.
pub fn segments_view(segments: &[Segment]) -> Vec<AnyView> {
    segments
        .iter()
        .map(|segment| match segment {
            Segment::Input {
                text,
                target: Some(Kind::Ingredient),
                ..
            }
            | Segment::Time { text }
            | Segment::Temp { text } => view! { <b>{text.clone()}</b> }.into_any(),
            other => other.text().to_owned().into_any(),
        })
        .collect()
}

/// "Last time" callouts for one step or ingredient.
pub fn last_time(store: Store, rec: &RecipeRecord, target: &str) -> Vec<AnyView> {
    rec.adjustments
        .iter()
        .filter(|adjustment| adjustment.target == target)
        .map(|adjustment: &Adjustment| {
            let (rec, id) = (rec.clone(), adjustment.id.clone());
            view! {
                <aside class="last-time">
                    <p><b>"Last time: "</b>{adjustment.text.clone()}</p>
                    <button class="link" on:click=move |_| store.dismiss_adjustment(&rec, &id)>"Dismiss"</button>
                </aside>
            }
            .into_any()
        })
        .collect()
}

/// Sizes a notes field to its text.
fn fit(area: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(area);
    let _ = style.set_property("height", "auto");
    let _ = style.set_property("height", &format!("{}px", area.scroll_height() + 2));
}

#[component]
fn Notes(id: String) -> impl IntoView {
    let store = use_store();
    let area = NodeRef::<html::Textarea>::new();
    let recipe = id.clone();
    // The field is not bound to the record, so typing never re-renders it;
    // it is only overwritten when the stored text differs from what it
    // shows (another device or an agent wrote, or the server kept both texts).
    Effect::new(move |_| {
        let stored = store
            .recipes
            .with(|recipes| recipes.get(&recipe).map(|recipe| recipe.rec.notes.clone()));
        if let (Some(stored), Some(area)) = (stored, area.get())
            && area.value() != stored
        {
            area.set_value(&stored);
            fit(&area);
        }
    });
    view! {
        <section class="notes">
            <h2>"Notes"</h2>
            <textarea
                node_ref=area
                rows="3"
                placeholder="Yours to write. Saved as you type."
                aria-label="Notes"
                on:input=move |event| {
                    store.set_notes(&id, event_target_value(&event));
                    if let Some(area) = area.get() {
                        fit(&area);
                    }
                }
                on:blur=move |_| store.flush()
            ></textarea>
        </section>
    }
}

#[component]
fn OriginalSection(rec: RecipeRecord, doc_url: Option<String>) -> impl IntoView {
    let store = use_store();
    let open = RwSignal::new(false);
    let loaded = RwSignal::new(None::<Option<Original>>);
    let source = rec.source.clone();
    let has_stored = source.is_some();
    let id = rec.id.clone();
    let toggle = move |_| {
        open.update(|open| *open = !*open);
        if open.get_untracked() && has_stored && loaded.get_untracked().is_none() {
            let id = id.clone();
            spawn_local(async move {
                let original = store.original(id).await;
                loaded.set(Some(original));
            });
        }
    };
    let link = source
        .as_ref()
        .and_then(|source| source.url.clone())
        .or(doc_url);
    let photos = source.as_ref().map_or(0, |source| source.files);
    let item = source.map(|source| source.item_id).unwrap_or_default();
    (has_stored || link.is_some()).then(|| view! {
        <section class="original">
            <button class="disclose" aria-expanded=move || open.get().to_string() on:click=toggle>
                <h2>"Original"</h2>
                <span class="chev" class:open=move || open.get()>{icon("next")}</span>
            </button>
            <Show when=move || open.get()>
                {link.clone().map(|url| view! {
                    <p><a class="ext" href=url.clone() target="_blank" rel="noopener noreferrer">{url.clone()}</a></p>
                })}
                {(0..photos)
                    .map(|n| view! { <img class="src-photo" src=paths::source_photo(&item, n) alt="Photo of the original" loading="lazy"/> })
                    .collect_view()}
                {move || match loaded.get() {
                    None if has_stored => view! { <p class="muted">"Loading…"</p> }.into_any(),
                    Some(None) => view! {
                        <p class="muted">"The stored original has not been viewed on this phone yet. It will load when Fond is back online."</p>
                    }.into_any(),
                    Some(Some(original)) => original
                        .text
                        .map(|text| view! { <pre class="src-text">{text}</pre> })
                        .into_any(),
                    None => ().into_any(),
                }}
            </Show>
        </section>
    })
}

#[component]
fn ScaleControl(id: String, doc: Arc<RecipeDoc>) -> impl IntoView {
    let store = use_store();
    let (get_id, set_id, step_id) = (id.clone(), id.clone(), id);
    let factor = move || store.scale(&get_id);
    let (yield_qty, unit) = (doc.r#yield.qty, doc.r#yield.unit.clone());
    let stepper = move |delta: i32| {
        let id = step_id.clone();
        move |_| store.set_scale(&id, step_servings(yield_qty, store.scale(&id), delta))
    };
    let servings = {
        let factor = factor.clone();
        move || omni_recipe_format::format::fmt_amount(Some(yield_qty * factor()), None)
    };
    let scaled = factor.clone();
    view! {
        <section class="scale" aria-label="Scale">
            <div class="seg" role="group" aria-label="Scale">
                {SCALES
                    .into_iter()
                    .map(|(preset, label)| {
                        let (factor, id) = (factor.clone(), set_id.clone());
                        view! {
                            <button class:on=move || same_scale(factor(), preset)
                                aria-pressed=move || same_scale(store.scale(&id), preset).to_string()
                                on:click={
                                    let id = set_id.clone();
                                    move |_| store.set_scale(&id, preset)
                                }>
                                {label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            <div class="stepper">
                <button aria-label="Fewer servings" on:click=stepper(-1)>"−"</button>
                <span data-testid="servings"><b>{servings}</b>" "{unit}</span>
                <button aria-label="More servings" on:click=stepper(1)>"+"</button>
            </div>
            <Show when=move || !same_scale(scaled(), 1.0)>
                <p class="scale-note">"Times, temperatures and pan sizes do not scale."</p>
            </Show>
        </section>
    }
}

/// One step in full, for the chart's tap sheet.
fn step_detail(
    store: Store,
    recipe: &Recipe,
    doc: &RecipeDoc,
    factor: f64,
    step_id: &str,
) -> Option<AnyView> {
    let list = list_view(doc, factor);
    let step = list.steps.iter().find(|step| step.id == step_id)?;
    let ix = Index::build(doc);
    let source = ix.step(step_id)?;
    Some(
        view! {
            <p class="step-no">{format!("Step {} of {}", step.number, list.steps.len())}</p>
            <p class="step-text">{segments_view(&step.segments)}</p>
            {source.until.clone().map(|until| view! { <p class="until"><b>"Until: "</b>{until}</p> })}
            {source.tip.clone().map(|tip| view! { <p class="tip">{tip}</p> })}
            {last_time(store, &recipe.rec, step_id)}
        }
        .into_any(),
    )
}

#[component]
fn ListBody(recipe: Arc<Recipe>, doc: Arc<RecipeDoc>, factor: f64) -> impl IntoView {
    let store = use_store();
    let list = list_view(&doc, factor);
    let ix = Index::build(&doc);
    let multi = doc.components.len() > 1;
    let groups = list
        .groups
        .iter()
        .map(|group| {
            let lines = group
                .lines
                .iter()
                .map(|line| {
                    let linked = ix.ingredient(&line.id).and_then(|ingredient| ingredient.recipe.clone());
                    let label = line.text.clone();
                    let text = match linked {
                        Some(linked) => view! { <Link to=Route::Recipe(linked) class="sub">{label}</Link> }.into_any(),
                        None => line.text.clone().into_any(),
                    };
                    view! {
                        <li class:optional=line.optional data-ingredient=line.id.clone()>
                            {text}
                            {last_time(store, &recipe.rec, &line.id)}
                        </li>
                    }
                })
                .collect_view();
            view! {
                {group.heading.clone().map(|heading| view! {
                    <h3>{heading}{group.make_ahead.clone().map(|note| view! { <small>{note}</small> })}</h3>
                })}
                <ul class="ingredients">
                    {lines}
                    {group.from_above.iter().map(|name| view! { <li class="from-above">{name.clone()}" (from above)"</li> }).collect_view()}
                </ul>
            }
        })
        .collect_view();
    let mut component = String::new();
    let steps = list
        .steps
        .iter()
        .map(|step| {
            let heading = (multi && step.component != component).then(|| {
                component.clone_from(&step.component);
                ix.component_of(&step.id)
                    .and_then(|component| component.name.clone())
                    .map(|name| view! { <h3>{name}</h3> })
            });
            let source = ix.step(&step.id);
            view! {
                {heading}
                <div class="step" class:optional=step.optional data-step=step.id.clone()>
                    <span class="num">{step.number}</span>
                    <div>
                        <p>{segments_view(&step.segments)}</p>
                        {source.and_then(|s| s.tip.clone()).map(|tip| view! { <p class="tip">{tip}</p> })}
                        {last_time(store, &recipe.rec, &step.id)}
                    </div>
                </div>
            }
        })
        .collect_view();
    view! {
        <section class="list">
            <h2>"Ingredients"</h2>
            {groups}
            <h2>"Steps"</h2>
            {steps}
        </section>
    }
}

fn same_page(a: &Option<Arc<Recipe>>, b: &Option<Arc<Recipe>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            // Notes have their own field; typing must not redraw the page.
            let strip = |rec: &RecipeRecord| RecipeRecord {
                notes: String::new(),
                notes_rev: 0,
                seq: 0,
                updated_at: 0,
                ..rec.clone()
            };
            strip(&a.rec) == strip(&b.rec)
        }
        (None, None) => true,
        _ => false,
    }
}

#[component]
pub fn RecipePage(id: String) -> impl IntoView {
    let store = use_store();
    let page_id = id.clone();
    let page = Memo::new_with_compare(
        move |_| {
            store
                .recipe(&page_id)
                .filter(|recipe| recipe.rec.state != RecipeState::Trashed)
        },
        |old, new| !old.is_some_and(|old| new.is_some_and(|new| same_page(old, new))),
    );
    let chart = RwSignal::new(false);
    let menu = RwSignal::new(false);
    let step_sheet = RwSignal::new(false);
    let step = RwSignal::new(String::new());
    let slide = RwSignal::new(0usize);

    let change_state = move |state: RecipeState, done: &'static str| {
        let Some(recipe) = page.get_untracked() else {
            return;
        };
        let before = recipe.rec.clone();
        store.set_state(&before, state);
        menu.set(false);
        let undo: Undo = Arc::new(move || store.set_state(&before, before.state));
        store.show_toast(done, Some(undo));
        if state != RecipeState::Active {
            replace(store, Route::Library);
        }
    };

    let cook_id = id.clone();
    let open_session = move || {
        store.sessions.with(|sessions| {
            sessions
                .values()
                .find(|session| session.recipe_id == cook_id && is_open(session))
                .map(|session| session.id.clone())
        })
    };
    let resume = open_session.clone();
    let scale_id = id.clone();
    let cook = move |_| {
        let Some(recipe) = page.get_untracked() else {
            return;
        };
        let session = open_session.clone()()
            .unwrap_or_else(|| store.start_cook(&recipe, store.scale(&recipe.rec.id)));
        navigate(store, Route::Cook(session));
    };

    view! {
        {move || {
            let Some(recipe) = page.get() else {
                return view! {
                    <main class="page empty">
                        <p>"This recipe is not in your library."</p>
                        <Link to=Route::Library class="btn">"Go to the library"</Link>
                    </main>
                }
                .into_any();
            };
            let rec = recipe.rec.clone();
            let images = gallery(&rec);
            let count = images.len();
            let art = if images.is_empty() {
                let (background, _) = store.tags.with_untracked(|tags| tile_color(&rec.tags, tags, &recipe.title));
                view! { <div class="band" style=format!("background-color:{background}")></div> }.into_any()
            } else {
                view! {
                    <div class="gallery-wrap">
                        <div class="gallery" data-testid="gallery" on:scroll=move |event| {
                            if let Some(strip) = event.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                                let width = f64::from(strip.client_width().max(1));
                                slide.set((f64::from(strip.scroll_left()) / width).round() as usize);
                            }
                        }>
                            {images
                                .iter()
                                .enumerate()
                                .map(|(n, image)| view! {
                                    <div class="slide" style=format!("background-color:{}", image.color)>
                                        <img src=paths::image(&image.digest, ImageVariant::Full)
                                            alt=image.caption.clone().unwrap_or_default()
                                            loading=if n == 0 { "eager" } else { "lazy" } decoding="async"/>
                                    </div>
                                })
                                .collect_view()}
                        </div>
                        {(count > 1).then(|| view! {
                            <span class="slide-count">{move || format!("{} / {count}", slide.get().min(count - 1) + 1)}</span>
                        })}
                    </div>
                }
                .into_any()
            };
            let Some(doc) = recipe.doc.clone() else {
                return view! {
                    <main class="page recipe">
                        <header class="bar">
                            <button class="icon-btn" aria-label="Back" on:click=move |_| back(store, Route::Library)>{icon("back")}</button>
                        </header>
                        <div class="body">
                            <h1>{recipe.title.clone()}</h1>
                            <p class="muted">"This recipe was saved by a newer version of Fond. Update the app to read it."</p>
                        </div>
                    </main>
                }
                .into_any();
            };
            let header = list_view(&doc, 1.0);
            let tools = equipment(&doc);
            let source = doc.source.clone();
            let source_line = [source.name.clone(), source.author.clone(), source.detail.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let (body_recipe, body_doc, body_id) = (recipe.clone(), doc.clone(), scale_id.clone());
            let (sheet_recipe, sheet_doc, sheet_id) = (recipe.clone(), doc.clone(), scale_id.clone());
            let archived = rec.state == RecipeState::Archived;
            view! {
                <main class="page recipe">
                    <header class="bar">
                        <button class="icon-btn" aria-label="Back" on:click=move |_| back(store, Route::Library)>{icon("back")}</button>
                        <Connection/>
                        <button class="icon-btn" aria-label="More" on:click=move |_| menu.set(true)>{icon("more")}</button>
                    </header>
                    {art}
                    <div class="body">
                        <h1>{recipe.title.clone()}</h1>
                        {header.summary.clone().map(|summary| view! { <p class="summary">{summary}</p> })}
                        <p class="meta">{header.time.clone()}</p>
                        {archived.then(|| view! { <p class="badge inline">"Archived"</p> })}
                        <p class="tags">
                            {rec.tags.iter().map(|tag| view! { <span class="tag">{tag.clone()}</span> }).collect_view()}
                        </p>
                        {(!source_line.is_empty()).then(|| view! { <p class="source">"From "{source_line}</p> })}

                        <ScaleControl id=scale_id.clone() doc=doc.clone()/>

                        <div class="seg wide" role="tablist">
                            <button role="tab" class:on=move || !chart.get() aria-selected=move || (!chart.get()).to_string()
                                on:click=move |_| chart.set(false)>"Steps"</button>
                            <button role="tab" class:on=move || chart.get() aria-selected=move || chart.get().to_string()
                                on:click=move |_| chart.set(true)>"Chart"</button>
                        </div>
                        {move || {
                            let factor = store.scale(&body_id);
                            if chart.get() {
                                view! {
                                    <section class="charts" data-testid="chart">
                                        <Chart doc=body_doc.clone() factor=factor on_tap=Callback::new(move |cell: Cell| {
                                            step.set(cell.step);
                                            step_sheet.set(true);
                                        })/>
                                        <p class="hint">"Tap a step to read it in full. Turn the phone sideways for long charts."</p>
                                    </section>
                                }
                                .into_any()
                            } else {
                                view! { <ListBody recipe=body_recipe.clone() doc=body_doc.clone() factor=factor/> }.into_any()
                            }
                        }}

                        {(!tools.is_empty()).then(|| view! {
                            <section class="need">
                                <h2>"You'll need"</h2>
                                <ul>{tools.iter().map(|tool| view! { <li>{tool.clone()}</li> }).collect_view()}</ul>
                            </section>
                        })}
                        <Notes id=rec.id.clone()/>
                        <OriginalSection rec=rec.clone() doc_url=source.url.clone()/>
                    </div>
                    <footer class="cta">
                        <button class="btn primary big" data-testid="cook" on:click=cook.clone()>
                            {
                                let resume = resume.clone();
                                move || if resume().is_some() { "Resume cooking" } else { "Cook" }
                            }
                        </button>
                    </footer>
                </main>
                <Sheet open=menu title="Recipe">
                    <div class="menu">
                        {if archived {
                            view! { <button on:click=move |_| change_state(RecipeState::Active, "Back in the library")>"Unarchive"</button> }.into_any()
                        } else {
                            view! { <button on:click=move |_| change_state(RecipeState::Archived, "Archived")>"Archive"</button> }.into_any()
                        }}
                        <button class="danger" on:click=move |_| change_state(RecipeState::Trashed, "Deleted")>"Delete"</button>
                    </div>
                </Sheet>
                <Sheet open=step_sheet title=Signal::derive({
                    let doc = sheet_doc.clone();
                    move || {
                        let id = step.get();
                        Index::build(&doc).step(&id).map(|step| step.action.clone()).unwrap_or_default()
                    }
                })>
                    {move || step_detail(store, &sheet_recipe, &sheet_doc, store.scale(&sheet_id), &step.get())}
                </Sheet>
            }
            .into_any()
        }}
    }
}
