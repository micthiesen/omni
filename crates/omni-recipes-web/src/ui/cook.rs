//! Cook mode: three views (Gather, Steps, Chart) of one session's event log.

use std::sync::Arc;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use omni_api::fond::CookEventKind;
use omni_recipe_format::format::fmt_time;
use omni_recipe_format::{
    Attention, Cell, CellKind, Index, ListView, Mark, RecipeDoc, cook_marks, gather_list,
    list_view, meanwhile, step_inputs,
};

use super::chart::{CellMark, Chart};
use super::recipe::segments_view;
use super::{Connection, Link, Sheet, back, icon, remaining, replace};
use crate::logic::Route;
use crate::logic::session::{Projection, project};
use crate::platform::{device, now_ms};
use crate::store::{Store, use_store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Gather,
    Steps,
    Chart,
}

/// What the Adjust sheet is editing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    id: String,
    title: String,
    original: String,
}

/// What every piece of the cook screen shares. The stored values belong to
/// one rendering of the session's document.
#[derive(Clone, Copy)]
struct Ctx {
    store: Store,
    session: StoredValue<String>,
    recipe: StoredValue<String>,
    state: Memo<Projection>,
    doc: StoredValue<Arc<RecipeDoc>>,
    list: StoredValue<Arc<ListView>>,
    scale: f64,
    chosen: RwSignal<Option<String>>,
    tab: RwSignal<Tab>,
    target: RwSignal<Option<Target>>,
    adjusting: RwSignal<bool>,
    showing_original: RwSignal<Option<String>>,
}

impl Ctx {
    fn send(self, kind: CookEventKind) {
        self.store.cook_event(&self.session.get_value(), kind);
    }

    fn toggle(self, target: &str) {
        self.send(self.state.with_untracked(|state| state.toggle(target)));
    }

    fn adjusted(self, target: &str) -> Option<String> {
        self.state.with(|state| state.adjusted.get(target).cloned())
    }

    fn open_adjust(self, target: Target) {
        self.target.set(Some(target));
        self.adjusting.set(true);
    }

    /// "Ragù: simmer" in a recipe with several components, else "simmer".
    fn step_label(self, step: &str) -> String {
        let doc = self.doc.get_value();
        let ix = Index::build(&doc);
        let Some(found) = ix.step(step) else {
            return step.to_owned();
        };
        match ix
            .component_of(step)
            .and_then(|component| component.name.as_deref())
        {
            Some(name) if doc.components.len() > 1 => format!("{name}: {}", found.action),
            _ => found.action.clone(),
        }
    }
}

/// The marker under an adjusted item (with its original one tap away) and
/// the previous cook's adjustments with "Use again".
fn adjust_notes(ctx: Ctx, target: String, original: String) -> impl IntoView {
    let (marker_target, last_target) = (target.clone(), target);
    let marker = move || {
        let target = marker_target.clone();
        ctx.adjusted(&target).map(|_| {
            let (toggle, shown) = (target.clone(), target.clone());
            let original = original.clone();
            view! {
                <p class="adjusted">
                    <span class="mark">"Adjusted"</span>
                    <button class="link" on:click=move |_| ctx.showing_original.update(|open| {
                        *open = if open.as_deref() == Some(toggle.as_str()) { None } else { Some(toggle.clone()) };
                    })>
                        {
                            let shown = shown.clone();
                            move || if ctx.showing_original.get().as_deref() == Some(shown.as_str()) { "Hide original" } else { "Original" }
                        }
                    </button>
                    <button class="link" on:click={
                        let target = target.clone();
                        move |_| ctx.send(CookEventKind::Unadjust { target: target.clone() })
                    }>"Undo"</button>
                </p>
                <Show when=move || ctx.showing_original.get().as_deref() == Some(shown.as_str())>
                    <p class="orig">{original.clone()}</p>
                </Show>
            }
        })
    };
    let last = move || {
        let target = last_target.clone();
        let current = ctx.adjusted(&target);
        let earlier = ctx
            .store
            .recipe(&ctx.recipe.get_value())
            .map(|recipe| recipe.rec.adjustments.clone())
            .unwrap_or_default();
        earlier
            .into_iter()
            .filter(|adjustment| {
                adjustment.target == target && current.as_deref() != Some(adjustment.text.as_str())
            })
            .map(|adjustment| {
                let (target, text) = (adjustment.target.clone(), adjustment.text.clone());
                view! {
                    <aside class="last-time">
                        <p><b>"Last time: "</b>{adjustment.text.clone()}</p>
                        <button class="link" on:click=move |_| ctx.send(CookEventKind::Adjust {
                            target: target.clone(),
                            text: text.clone(),
                        })>"Use again"</button>
                    </aside>
                }
            })
            .collect_view()
    };
    view! { {marker}{last} }
}

fn gather(ctx: Ctx) -> impl IntoView {
    gather_list(&ctx.doc.get_value(), ctx.scale)
        .into_iter()
        .map(|group| {
            let lines = group
                .lines
                .into_iter()
                .map(|line| {
                    let (id, done_id, text_id) = (line.id.clone(), line.id.clone(), line.id.clone());
                    let (original, edit_original, edit_id) = (line.text.clone(), line.text.clone(), line.id.clone());
                    view! {
                        <li class:optional=line.optional>
                            <div class="row">
                                <button class="check" data-target=line.id.clone()
                                    class:done=move || ctx.state.with(|state| state.checked.contains(&done_id))
                                    on:click=move |_| ctx.toggle(&id)>
                                    <span class="box">{icon("check")}</span>
                                    <span class="txt">{move || ctx.adjusted(&text_id).unwrap_or_else(|| original.clone())}</span>
                                </button>
                                <button class="icon-btn quiet" aria-label="Adjust" on:click=move |_| ctx.open_adjust(Target {
                                    id: edit_id.clone(),
                                    title: "Adjust ingredient".to_owned(),
                                    original: edit_original.clone(),
                                })>{icon("edit")}</button>
                            </div>
                            {adjust_notes(ctx, line.id.clone(), line.text.clone())}
                        </li>
                    }
                })
                .collect_view();
            view! {
                {group.heading.map(|heading| view! {
                    <h3>{heading}{group.make_ahead.map(|note| view! { <small>{note}</small> })}</h3>
                })}
                <ul class="gather">
                    {lines}
                    {group.from_above.into_iter().map(|name| view! { <li class="from-above">{name}" (from above)"</li> }).collect_view()}
                </ul>
            }
        })
        .collect_view()
}

/// The strip for running timers: the countdown and what can be done meanwhile.
fn running(ctx: Ctx) -> impl IntoView {
    move || {
        let state = ctx.state.get();
        let doc = ctx.doc.get_value();
        let done = state.done_steps(&doc);
        state
            .timers
            .iter()
            .filter(|(step, _)| !done.contains(step))
            .map(|(step, ends_at)| {
                let ends_at = *ends_at;
                let passive = Index::build(&doc)
                    .step(step)
                    .and_then(|found| found.time)
                    .is_some_and(|time| time.attention == Attention::Passive);
                let offers = if passive { meanwhile(&doc, step, &done) } else { Vec::new() };
                let (open, label) = (step.clone(), ctx.step_label(step));
                let chart_label = Index::build(&doc)
                    .step(step)
                    .map(omni_recipe_format::chart_label)
                    .unwrap_or_default();
                view! {
                    <div class="running" data-testid="running">
                        <button class="running-main" on:click=move |_| {
                            ctx.chosen.set(Some(open.clone()));
                            ctx.tab.set(Tab::Steps);
                        }>
                            {icon("timer")}
                            <span>{label}</span>
                            <b class:late={move || ends_at <= ctx.store.now.get()}>{move || remaining(ctx.store, ends_at)}</b>
                        </button>
                        {(!offers.is_empty()).then(|| view! {
                            <p class="meanwhile">
                                {format!("While «{chart_label}» runs: ")}
                                {offers
                                    .into_iter()
                                    .take(3)
                                    .map(|offer| {
                                        let label = ctx.step_label(&offer);
                                        view! {
                                            <button class="chip" on:click=move |_| {
                                                ctx.chosen.set(Some(offer.clone()));
                                                ctx.tab.set(Tab::Steps);
                                            }>{label}</button>
                                        }
                                    })
                                    .collect_view()}
                            </p>
                        })}
                    </div>
                }
            })
            .collect_view()
    }
}

fn step_card(ctx: Ctx) -> impl IntoView {
    move || {
        let state = ctx.state.get();
        let doc = ctx.doc.get_value();
        let list = ctx.list.get_value();
        let ix = Index::build(&doc);
        // A step the cook opened stays open even once it is done.
        let chosen = ctx.chosen.get().filter(|id| ix.step(id).is_some());
        let current = chosen.or_else(|| state.current_step(&doc, None))?;
        let step = ix.step(&current)?;
        let listed = list.steps.iter().find(|listed| listed.id == current)?;
        let total = list.steps.len();
        let is_done = state.checked.contains(&current);
        let next = state.next_after(&doc, &current);
        let all_done = state.progress(&doc).0 == total;
        let before = (listed.number > 1).then(|| list.steps[listed.number - 2].id.clone());
        let after = list.steps.get(listed.number).map(|step| step.id.clone());
        let adjusted = state.adjusted.get(&current).cloned();
        let text = match &adjusted {
            Some(text) => text.clone().into_any(),
            None => segments_view(&listed.segments).into_any(),
        };
        let chips = step_inputs(&ix, step, ctx.scale)
            .into_iter()
            .map(|input| {
                let on = state.input_checked(&current, &input.key);
                let key = input.key.clone();
                view! {
                    <button class="chip input" class:on=on class:optional=input.optional data-key=input.key.clone()
                        aria-pressed=on.to_string() on:click=move |_| ctx.toggle(&key)>
                        {input.label}
                    </button>
                }
            })
            .collect_view();
        let timer = step.time.map(|time| {
            let (start, cancel) = (current.clone(), current.clone());
            let minutes = time.longest();
            match state.timers.get(&current).copied() {
                Some(ends_at) => view! {
                    <div class="timer on">
                        {icon("timer")}
                        <b data-testid="countdown" class:late={move || ends_at <= ctx.store.now.get()}>
                            {move || remaining(ctx.store, ends_at)}
                        </b>
                        <button class="link" on:click=move |_| ctx.send(CookEventKind::CancelTimer { step: cancel.clone() })>
                            "Cancel"
                        </button>
                    </div>
                }
                .into_any(),
                None => view! {
                    <button class="timer" data-testid="timer" on:click=move |_| {
                        device::prime_audio();
                        ctx.send(CookEventKind::Timer {
                            step: start.clone(),
                            ends_at: now_ms() + i64::from(minutes) * 60_000,
                        });
                    }>
                        {icon("timer")}
                        {format!("Start {} timer", fmt_time(minutes, None))}
                    </button>
                }
                .into_any(),
            }
        });
        let (done_id, edit_id, edit_text) = (current.clone(), current.clone(), listed.text.clone());
        let go_next = next.clone();
        let next_line = next.as_ref().map(|id| ctx.step_label(id));
        Some(view! {
            <article class="card-step" class:done=is_done data-step=current.clone()>
                <header>
                    <button class="icon-btn quiet" aria-label="Previous step" disabled=before.is_none()
                        on:click=move |_| ctx.chosen.set(before.clone())>{icon("back")}</button>
                    <span class="step-no">{format!("Step {} of {total}", listed.number)}{step.optional.then_some(" · optional")}</span>
                    <button class="icon-btn quiet" aria-label="Following step" disabled=after.is_none()
                        on:click=move |_| ctx.chosen.set(after.clone())>{icon("next")}</button>
                </header>
                <h2>{ctx.step_label(&current)}</h2>
                <p class="step-text" data-testid="step-text">{text}</p>
                {adjust_notes(ctx, current.clone(), listed.text.clone())}
                <div class="chips">{chips}</div>
                {step.until.clone().map(|until| view! { <p class="until"><b>"Until "</b>{until}</p> })}
                {step.tip.clone().map(|tip| view! { <p class="tip">{tip}</p> })}
                {timer}
                <div class="card-actions">
                    <button class="btn ghost" data-testid="adjust" on:click=move |_| ctx.open_adjust(Target {
                        id: edit_id.clone(),
                        title: "Adjust step".to_owned(),
                        original: edit_text.clone(),
                    })>{icon("edit")}"Adjust"</button>
                    {if is_done {
                        view! {
                            <button class="btn ghost" on:click=move |_| ctx.toggle(&done_id)>"Not done"</button>
                        }.into_any()
                    } else {
                        view! {
                            <button class="btn primary" data-testid="done" on:click=move |_| {
                                ctx.toggle(&done_id);
                                ctx.chosen.set(go_next.clone());
                            }>{icon("check")}"Done"</button>
                        }.into_any()
                    }}
                </div>
            </article>
            <p class="next-line" data-testid="next">
                {match next_line {
                    Some(label) if !is_done => format!("Next: {label}"),
                    Some(label) => format!("Ready: {label}"),
                    None if all_done || (!is_done && state.progress(&doc).0 + 1 == total) => "This is the last step.".to_owned(),
                    None => "Nothing else is ready until this is done.".to_owned(),
                }}
            </p>
        })
    }
}

#[component]
pub fn CookPage(id: String) -> impl IntoView {
    let store = use_store();
    let session_id = StoredValue::new(id.clone());
    let lookup = id.clone();
    let session = Memo::new(move |_| {
        store
            .sessions
            .with(|sessions| sessions.get(&lookup).cloned())
    });
    let state = Memo::new(move |_| session.with(|s| s.as_ref().map(project).unwrap_or_default()));
    // The document only changes on a rebase or a new head revision, so the
    // screen below is built once and checks only restyle it.
    let basis = Memo::new(move |_| {
        session.with(|session| {
            let session = session.as_ref()?;
            let (doc, outdated) = store.session_doc(session, &state.get())?;
            Some((doc, outdated, session.scale, session.recipe_id.clone()))
        })
    });
    let finished = Memo::new(move |_| state.with(|state| state.finished));
    let tab = RwSignal::new(Tab::Steps);
    let chosen = RwSignal::new(None::<String>);
    let target = RwSignal::new(None::<Target>);
    let adjusting = RwSignal::new(false);
    let finishing = RwSignal::new(false);
    let showing_original = RwSignal::new(None::<String>);
    let adjust_area = NodeRef::<html::Textarea>::new();
    let finish_note = NodeRef::<html::Input>::new();
    let finish_photo = NodeRef::<html::Input>::new();
    let photo_name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    if session.get_untracked().is_none() {
        spawn_local(async move { store.refresh_session(id).await });
    }
    Effect::new(move |_| {
        if adjusting.get()
            && let (Some(target), Some(area)) = (target.get(), adjust_area.get())
        {
            let text = state.with_untracked(|state| state.adjusted.get(&target.id).cloned());
            area.set_value(&text.unwrap_or(target.original));
        }
    });

    let send = move |kind: CookEventKind| store.cook_event(&session_id.get_value(), kind);
    let recipe_route = move || {
        session
            .with_untracked(|s| s.as_ref().map(|s| s.recipe_id.clone()))
            .map_or(Route::Library, Route::Recipe)
    };
    let save_adjustment = move |_| {
        let (Some(target), Some(area)) = (target.get_untracked(), adjust_area.get_untracked())
        else {
            return;
        };
        let text = area.value().trim().to_owned();
        if text.is_empty() || text == target.original {
            if state.with_untracked(|state| state.adjusted.contains_key(&target.id)) {
                send(CookEventKind::Unadjust { target: target.id });
            }
        } else {
            send(CookEventKind::Adjust {
                target: target.id,
                text,
            });
        }
        adjusting.set(false);
    };
    let finish = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let note = finish_note
            .get_untracked()
            .map(|input| input.value().trim().to_owned())
            .filter(|note| !note.is_empty());
        let photo = finish_photo
            .get_untracked()
            .and_then(|input| input.files())
            .and_then(|files| files.get(0));
        let after = recipe_route();
        spawn_local(async move {
            // The photo is queued first so it reaches the server before the finish.
            if let Some(photo) = photo
                && !store.add_cook_photo(&session_id.get_value(), &photo).await
            {
                store.show_toast(
                    "The photo could not be read; the cook was logged without it.",
                    None,
                );
            }
            send(CookEventKind::Finish { note });
            busy.set(false);
            finishing.set(false);
            replace(store, after);
            store.show_toast("Cook logged", None);
        });
    };

    view! {
        <main class="page cook">
            <header class="bar">
                <button class="icon-btn" aria-label="Leave cook mode" on:click=move |_| back(store, recipe_route())>{icon("close")}</button>
                <h1>{move || {
                    session
                        .with(|s| s.as_ref().and_then(|s| store.recipe(&s.recipe_id)))
                        .map(|recipe| recipe.title.clone())
                }}</h1>
                <Connection/>
                <Show when=move || store.wake_refused.get()>
                    <span class="conn" data-testid="wake">"Screen may sleep"</span>
                </Show>
                <Show when=move || basis.with(Option::is_some) && !finished.get()>
                    <button class="btn small" data-testid="finish" on:click=move |_| finishing.set(true)>"Finish"</button>
                </Show>
            </header>
            {move || {
                if finished.get() {
                    return view! {
                        <div class="empty">
                            <p>"This cook is finished."</p>
                            <Link to=recipe_route() class="btn">"Back to the recipe"</Link>
                        </div>
                    }
                    .into_any();
                }
                let Some((doc, outdated, scale, recipe_id)) = basis.get() else {
                    return view! {
                        <div class="empty">
                            <p>"This cook session is not on this phone."</p>
                            <Link to=Route::Library class="btn">"Go to the library"</Link>
                        </div>
                    }
                    .into_any();
                };
                let ctx = Ctx {
                    store,
                    session: session_id,
                    recipe: StoredValue::new(recipe_id.clone()),
                    state,
                    list: StoredValue::new(Arc::new(list_view(&doc, scale))),
                    doc: StoredValue::new(doc.clone()),
                    scale,
                    chosen,
                    tab,
                    target,
                    adjusting,
                    showing_original,
                };
                let head_revision = store
                    .recipes
                    .with_untracked(|recipes| recipes.get(&recipe_id).map_or(0, |recipe| recipe.rec.revision));
                let marks = Callback::new(move |cell: Cell| {
                    ctx.state.with(|state| {
                        let doc = ctx.doc.get_value();
                        let step_mark = cook_marks(&doc, &state.done_steps(&doc)).get(&cell.step).copied();
                        match cell.kind {
                            CellKind::Step | CellKind::Setup => match step_mark {
                                Some(Mark::Done) => CellMark::Done,
                                Some(Mark::Ready) => CellMark::Ready,
                                None => CellMark::None,
                            },
                            CellKind::Leaf | CellKind::Ref => {
                                if state.input_checked(&cell.step, &cell.key) {
                                    CellMark::Done
                                } else {
                                    CellMark::None
                                }
                            }
                        }
                    })
                });
                let tapped = Callback::new(move |cell: Cell| ctx.toggle(&cell.key));
                let tab_button = move |this: Tab, label: &'static str| view! {
                    <button role="tab" class:on=move || tab.get() == this aria-selected=move || (tab.get() == this).to_string()
                        on:click=move |_| tab.set(this)>{label}</button>
                };
                view! {
                    <nav class="seg wide tabs" role="tablist">
                        {tab_button(Tab::Gather, "Gather")}
                        {tab_button(Tab::Steps, "Steps")}
                        {tab_button(Tab::Chart, "Chart")}
                    </nav>
                    {outdated.then(|| view! {
                        <div class="notice inline" data-testid="outdated">
                            <span class="notice-main">"The recipe was updated since you started."</span>
                            <button class="notice-act" on:click=move |_| send(CookEventKind::Rebase { revision: head_revision })>
                                "Switch"
                            </button>
                        </div>
                    })}
                    {move || {
                        state
                            .with(|state| state.notes.clone())
                            .into_iter()
                            .map(|note| view! { <p class="session-note">{note}</p> })
                            .collect_view()
                    }}
                    {running(ctx)}
                    {move || match tab.get() {
                        Tab::Gather => view! { <section class="tab gather-tab">{gather(ctx)}</section> }.into_any(),
                        Tab::Steps => view! { <section class="tab steps-tab">{step_card(ctx)}</section> }.into_any(),
                        Tab::Chart => view! {
                            <section class="tab charts live" data-testid="chart">
                                <Chart doc=ctx.doc.get_value() factor=scale mark=marks on_tap=tapped leaf_taps=true/>
                                <p class="hint">"Tap an ingredient to cross it off and a step to mark it done."</p>
                            </section>
                        }
                        .into_any(),
                    }}
                }
                .into_any()
            }}
        </main>
        <Sheet open=adjusting title=Signal::derive(move || target.get().map(|t| t.title).unwrap_or_default())>
            <p class="muted">"For this cook only. The recipe itself does not change."</p>
            <textarea node_ref=adjust_area class="adjust-text" rows="5" aria-label="Adjusted text"></textarea>
            <div class="sheet-actions">
                <button class="btn ghost" on:click=move |_| {
                    if let (Some(target), Some(area)) = (target.get_untracked(), adjust_area.get_untracked()) {
                        area.set_value(&target.original);
                    }
                }>"Reset to original"</button>
                <button class="btn primary" data-testid="save-adjust" on:click=save_adjustment>"Save"</button>
            </div>
        </Sheet>
        <Sheet open=finishing title="Finish cooking">
            {move || {
                let adjusted = state.with(|state| state.adjusted.clone());
                if adjusted.is_empty() {
                    view! { <p class="muted">"Cooked as written: nothing was adjusted."</p> }.into_any()
                } else {
                    view! {
                        <h3 class="group">"Adjusted this time"</h3>
                        <ul class="adjusted-list">
                            {adjusted.into_values().map(|text| view! { <li>{text}</li> }).collect_view()}
                        </ul>
                        <p class="muted">"These stay on the recipe as “Last time” until an agent folds them in."</p>
                    }
                    .into_any()
                }
            }}
            <label class="field">
                <span>"How did it go?"</span>
                <input node_ref=finish_note type="text" maxlength="200" placeholder="One line, optional" enterkeyhint="done"/>
            </label>
            <label class="photo-pick">
                {icon("camera")}
                <span>{move || {
                    let name = photo_name.get();
                    if name.is_empty() { "Add a photo of the result".to_owned() } else { name }
                }}</span>
                <input node_ref=finish_photo type="file" accept="image/*" class="visually-hidden"
                    on:change=move |_| {
                        let name = finish_photo
                            .get_untracked()
                            .and_then(|input| input.files())
                            .and_then(|files| files.get(0))
                            .map(|file| file.name())
                            .unwrap_or_default();
                        photo_name.set(name);
                    }/>
            </label>
            <div class="sheet-actions">
                <button class="btn primary" data-testid="finish-confirm" disabled=move || busy.get() on:click=finish>
                    "Finish"
                </button>
            </div>
        </Sheet>
    }
}
