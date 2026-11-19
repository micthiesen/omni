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

use super::amount::{AmountSheet, AmountTarget};
use super::chart::{CellMark, Chart};
use super::recipe::{Notes, segments_view};
use super::requests::{AskSheet, kitchen_items};
use super::{Connection, Link, Sheet, back, icon, remaining, replace};
use crate::logic::Route;
use crate::logic::session::{Projection, project};
use crate::platform::prefs::{self, RecipeView};
use crate::platform::{device, now_ms};
use crate::store::{Ask, Store, Undo, use_store};

/// How long the toast after a press and hold stays.
const HOLD_TOAST: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Gather,
    Steps,
    Chart,
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
    amount_open: RwSignal<bool>,
    amount_target: RwSignal<Option<AmountTarget>>,
    /// The ingredients of the step whose amounts are being chosen from: id
    /// and name.
    amount_choices: RwSignal<Vec<(String, String)>>,
    choosing_amount: RwSignal<bool>,
    notes_open: RwSignal<bool>,
    /// The chart's step sheet and the step it shows
    /// (docs/fond/cook-chart-steps.md).
    step_open: RwSignal<bool>,
    sheet_step: RwSignal<String>,
}

/// "Ragù: simmer" in a recipe with several components, else "simmer".
fn step_label(doc: &RecipeDoc, step: &str) -> String {
    let ix = Index::build(doc);
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

impl Ctx {
    fn send(self, kind: CookEventKind) {
        self.store.cook_event(&self.session.get_value(), kind);
    }

    fn toggle(self, target: &str) {
        self.send(self.state.with_untracked(|state| state.toggle(target)));
    }

    /// Marks a step done, or not done again. A step that is done has no use
    /// for its timer, so the timer stops with it instead of ringing later for
    /// a step the screen no longer shows as running. With `announce` (the
    /// chart's press and hold, where the finger covers the cell) a short
    /// toast names what happened and takes it back.
    fn toggle_step(self, step: &str, announce: bool) {
        let (was_done, timer) = self.state.with_untracked(|state| {
            (
                state.checked.contains(step),
                state.timers.get(step).copied(),
            )
        });
        self.toggle(step);
        let stopped = timer.filter(|_| !was_done);
        if stopped.is_some() {
            self.send(CookEventKind::CancelTimer {
                step: step.to_owned(),
            });
        }
        if announce {
            let label = self.step_label(step);
            let text = if was_done {
                format!("Not done: {label}")
            } else {
                format!("Done: {label}")
            };
            let step = step.to_owned();
            let undo: Undo = Arc::new(move || {
                let now_done = self
                    .state
                    .with_untracked(|state| state.checked.contains(&step));
                if now_done != was_done {
                    self.toggle(&step);
                }
                if let Some(ends_at) = stopped {
                    self.send(CookEventKind::Timer {
                        step: step.clone(),
                        ends_at,
                    });
                }
            });
            self.store.show_toast_for(text, Some(undo), HOLD_TOAST);
        }
    }

    /// Stops a step's timer; the toast starts it again with the same end.
    fn stop_timer(self, step: &str) {
        let Some(ends_at) = self
            .state
            .with_untracked(|state| state.timers.get(step).copied())
        else {
            return;
        };
        self.send(CookEventKind::CancelTimer {
            step: step.to_owned(),
        });
        // One that has run out was stopped on purpose.
        if ends_at > now_ms() {
            let step = step.to_owned();
            let undo: Undo = Arc::new(move || {
                self.send(CookEventKind::Timer {
                    step: step.clone(),
                    ends_at,
                });
            });
            self.store.show_toast("Timer cancelled", Some(undo));
        }
    }

    /// Shows a step: its sheet over the chart, its card on the other tabs.
    fn open_step(self, step: String) {
        if self.tab.get_untracked() == Tab::Chart {
            self.sheet_step.set(step);
            self.step_open.set(true);
        } else {
            self.chosen.set(Some(step));
            self.tab.set(Tab::Steps);
        }
    }

    /// Opens the amount sheet for an ingredient. What it saves is a revision
    /// of the recipe, which this session then moves to.
    fn change_amount(self, ingredient: String) {
        self.amount_target.set(Some(AmountTarget {
            recipe: self.recipe.get_value(),
            ingredient,
            session: Some(self.session.get_value()),
        }));
        self.choosing_amount.set(false);
        self.amount_open.set(true);
    }

    /// Those of `ids` that are ingredients with an amount, with their names.
    /// An ingredient to taste has no amount to change.
    fn amounts(self, ids: &[String]) -> Vec<(String, String)> {
        let doc = self.doc.get_value();
        let ix = Index::build(&doc);
        let mut found: Vec<(String, String)> = Vec::new();
        for id in ids {
            if let Some(ingredient) = ix.ingredient(id).filter(|i| i.qty.is_some())
                && !found.iter().any(|(seen, _)| seen == id)
            {
                found.push((ingredient.id.clone(), ingredient.name.clone()));
            }
        }
        found
    }

    fn step_label(self, step: &str) -> String {
        step_label(&self.doc.get_value(), step)
    }
}

fn gather(ctx: Ctx) -> impl IntoView {
    gather_list(&ctx.doc.get_value(), ctx.scale)
        .into_iter()
        .map(|group| {
            let lines = group
                .lines
                .into_iter()
                .map(|line| {
                    let (id, done_id) = (line.id.clone(), line.id.clone());
                    let editable = ctx.amounts(std::slice::from_ref(&line.id)).pop();
                    view! {
                        <li class:optional=line.optional>
                            <div class="row">
                                <button class="check" data-target=line.id.clone()
                                    class:done=move || ctx.state.with(|state| state.checked.contains(&done_id))
                                    on:click=move |_| ctx.toggle(&id)>
                                    <span class="box">{icon("check")}</span>
                                    <span class="txt">{line.text.clone()}</span>
                                </button>
                                {editable.map(|(ingredient, name)| view! {
                                    <button class="icon-btn quiet" data-testid="edit-amount"
                                        aria-label=format!("Change amount of {name}")
                                        on:click=move |_| ctx.change_amount(ingredient.clone())>{icon("edit")}</button>
                                })}
                            </div>
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

/// The running timers, docked under the screen: starting, stopping or
/// finishing one never moves what is being read or pressed above it.
fn dock(ctx: Ctx) -> impl IntoView {
    move || {
        let timers: Vec<(String, i64)> = ctx.state.with(|state| {
            state
                .timers
                .iter()
                .filter(|(step, _)| !state.checked.contains(*step))
                .map(|(step, ends_at)| (step.clone(), *ends_at))
                .collect()
        });
        (!timers.is_empty()).then(|| {
            let pills = timers
                .into_iter()
                .map(|(step, ends_at)| {
                    let label = ctx.step_label(&step);
                    view! {
                        <button class="running" data-testid="running" data-step=step.clone()
                            class:late=move || ends_at < ctx.store.now.get()
                            on:click=move |_| ctx.open_step(step.clone())>
                            {icon("timer")}
                            <span>{label}</span>
                            <b>{move || remaining(ctx.store, ends_at)}</b>
                        </button>
                    }
                })
                .collect_view();
            view! { <div class="timer-dock" data-testid="timer-dock">{pills}</div> }
        })
    }
}

/// What can be done while a passive step's timer runs, above the step card.
fn meanwhile_offers(ctx: Ctx) -> impl IntoView {
    move || {
        let state = ctx.state.get();
        let doc = ctx.doc.get_value();
        let ix = Index::build(&doc);
        let done = state.done_steps(&doc);
        state
            .timers
            .keys()
            .filter(|step| !done.contains(step))
            .filter_map(|step| {
                let found = ix.step(step)?;
                let passive = found
                    .time
                    .is_some_and(|time| time.attention == Attention::Passive);
                let offers = if passive {
                    meanwhile(&doc, step, &done)
                } else {
                    Vec::new()
                };
                if offers.is_empty() {
                    return None;
                }
                let chart_label = omni_recipe_format::chart_label(found);
                Some(view! {
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
                })
            })
            .collect_view()
    }
}

/// What the Steps tab's card and the chart's step sheet both show of one
/// step: its text at the session's scale, its inputs to cross off, the cue,
/// the tip, the timer, and the ways to the recipe's amounts and notes.
/// `in_sheet` closes the step sheet before another sheet opens over it.
fn step_body(ctx: Ctx, state: &Projection, id: &str, in_sheet: bool) -> Option<AnyView> {
    let doc = ctx.doc.get_value();
    let list = ctx.list.get_value();
    let ix = Index::build(&doc);
    let step = ix.step(id)?;
    let listed = list.steps.iter().find(|listed| listed.id == id)?;
    let leave_sheet = move || {
        if in_sheet {
            ctx.step_open.set(false);
        }
    };
    let chips = step_inputs(&ix, step, ctx.scale)
        .into_iter()
        .map(|input| {
            let on = state.input_checked(id, &input.key);
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
        let (start, cancel) = (id.to_owned(), id.to_owned());
        let minutes = time.longest();
        match state.timers.get(id).copied() {
            Some(ends_at) => view! {
                <div class="timer on">
                    {icon("timer")}
                    <b data-testid="countdown" class:late={move || ends_at < ctx.store.now.get()}>
                        {move || remaining(ctx.store, ends_at)}
                    </b>
                    <button class="btn small" data-testid="timer-stop" on:click=move |_| ctx.stop_timer(&cancel)>
                        {move || if ends_at < ctx.store.now.get() { "Stop" } else { "Cancel" }}
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
    let inputs: Vec<String> = step
        .inputs
        .iter()
        .map(|input| input.r#ref.clone())
        .collect();
    let amounts = ctx.amounts(&inputs);
    Some(
        view! {
            <p class="step-text" data-testid="step-text">{segments_view(&listed.segments)}</p>
            <div class="chips">{chips}</div>
            {step.until.clone().map(|until| view! { <p class="until"><b>"Until "</b>{until}</p> })}
            {step.tip.clone().map(|tip| view! { <p class="tip">{tip}</p> })}
            {timer}
            <div class="step-tools">
                {(!amounts.is_empty()).then(|| view! {
                    <button class="btn ghost small" data-testid="step-amounts" on:click=move |_| {
                        leave_sheet();
                        // One ingredient needs no list to choose from.
                        if let [(ingredient, _)] = amounts.as_slice() {
                            ctx.change_amount(ingredient.clone());
                        } else {
                            ctx.amount_choices.set(amounts.clone());
                            ctx.choosing_amount.set(true);
                        }
                    }>{icon("edit")}"Amounts"</button>
                })}
                <button class="btn ghost small" data-testid="recipe-notes" on:click=move |_| {
                    leave_sheet();
                    ctx.notes_open.set(true);
                }>"Recipe notes"</button>
            </div>
        }
        .into_any(),
    )
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
        let body = step_body(ctx, &state, &current, false)?;
        let done_id = current.clone();
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
                {body}
                <div class="card-actions">
                    {if is_done {
                        view! {
                            <button class="btn ghost" on:click=move |_| ctx.toggle_step(&done_id, false)>"Not done"</button>
                        }.into_any()
                    } else {
                        view! {
                            <button class="btn primary" data-testid="done" on:click=move |_| {
                                ctx.toggle_step(&done_id, false);
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

/// The sheet a step cell of the chart opens: the step as the Steps tab has
/// it, and one large button. Nothing when the step is not in the document
/// (the session moved to a revision without it).
fn step_sheet(ctx: Ctx) -> impl IntoView {
    move || {
        let state = ctx.state.get();
        let id = ctx.sheet_step.get();
        let doc = ctx.doc.get_value();
        let list = ctx.list.get_value();
        let optional = Index::build(&doc).step(&id)?.optional;
        let number = list.steps.iter().find(|listed| listed.id == id)?.number;
        let is_done = state.checked.contains(&id);
        let body = step_body(ctx, &state, &id, true)?;
        let toggle_id = id.clone();
        Some(view! {
            <div class="step-sheet" class:done=is_done data-testid="step-sheet" data-step=id>
                <p class="step-no">
                    {format!("Step {number} of {}", list.steps.len())}
                    {optional.then_some(" · optional")}
                    {is_done.then(|| view! { <span class="step-done">" · done"</span> })}
                </p>
                {body}
                <div class="card-actions">
                    {if is_done {
                        // Still to do after all: the step stays in front.
                        view! {
                            <button class="btn" data-testid="sheet-undo" on:click=move |_| ctx.toggle_step(&toggle_id, false)>"Undo"</button>
                        }.into_any()
                    } else {
                        // Back to the chart, where the cell is now dimmed.
                        view! {
                            <button class="btn primary" data-testid="sheet-done" on:click=move |_| {
                                ctx.toggle_step(&toggle_id, false);
                                ctx.step_open.set(false);
                            }>{icon("check")}"Mark done"</button>
                        }.into_any()
                    }}
                </div>
            </div>
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
            let (doc, _) = store.session_doc(session, &state.get())?;
            Some((doc, session.scale, session.recipe_id.clone()))
        })
    });
    // The recipe has a newer revision than the one being cooked from. The
    // cook stays on its own; the newer one is a tap away.
    let outdated = Memo::new(move |_| {
        session.with(|session| {
            session
                .as_ref()
                .and_then(|session| store.session_doc(session, &state.get()))
                .is_some_and(|(_, outdated)| outdated)
        })
    });
    let finished = Memo::new(move |_| state.with(|state| state.finished));
    let recipe_id = Memo::new(move |_| session.with(|s| s.as_ref().map(|s| s.recipe_id.clone())));
    let tab = RwSignal::new(match prefs::recipe_view() {
        Some(RecipeView::Chart) => Tab::Chart,
        _ => Tab::Steps,
    });
    let chosen = RwSignal::new(None::<String>);
    let amount_choices = RwSignal::new(Vec::<(String, String)>::new());
    let choosing_amount = RwSignal::new(false);
    let notes_open = RwSignal::new(false);
    let finishing = RwSignal::new(false);
    let discarding = RwSignal::new(false);
    let leaving = RwSignal::new(false);
    let leave_discarding = RwSignal::new(false);
    let amount_open = RwSignal::new(false);
    let amount_target = RwSignal::new(None::<AmountTarget>);
    let finish_note = NodeRef::<html::Input>::new();
    let finish_photo = NodeRef::<html::Input>::new();
    let photo_name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let menu = RwSignal::new(false);
    let asking = RwSignal::new(false);
    let step_open = RwSignal::new(false);
    let sheet_step = RwSignal::new(String::new());
    // The pieces of one rendering of the session's document.
    let screen = move |doc: Arc<RecipeDoc>, scale: f64, recipe: String| Ctx {
        store,
        session: session_id,
        recipe: StoredValue::new(recipe),
        state,
        list: StoredValue::new(Arc::new(list_view(&doc, scale))),
        doc: StoredValue::new(doc),
        scale,
        chosen,
        tab,
        amount_open,
        amount_target,
        amount_choices,
        choosing_amount,
        notes_open,
        step_open,
        sheet_step,
    };
    // The step sheet has nothing to show once its step is gone (a rebase to
    // a revision without it) or the cook is over.
    Effect::new(move |_| {
        if !step_open.get() {
            return;
        }
        let step = sheet_step.get();
        let exists = !finished.get()
            && basis.with(|basis| {
                basis
                    .as_ref()
                    .is_some_and(|(doc, ..)| Index::build(doc).step(&step).is_some())
            });
        if !exists {
            step_open.set(false);
        }
    });

    let offer = StoredValue::new(None::<u64>);
    let withdraw = move || {
        if let Some(id) = offer.get_value() {
            offer.set_value(None);
            store.withdraw_reload(id);
        }
    };
    Effect::new(move |_| {
        if outdated.get() && !finished.get() {
            if offer.get_value().is_none() {
                let reload: Undo = Arc::new(move || {
                    let head = session.with_untracked(|session| {
                        let session = session.as_ref()?;
                        store.recipes.with_untracked(|recipes| {
                            recipes
                                .get(&session.recipe_id)
                                .map(|recipe| recipe.rec.revision)
                        })
                    });
                    if let Some(revision) = head {
                        store.cook_event(
                            &session_id.get_value(),
                            CookEventKind::Rebase { revision },
                        );
                    }
                });
                offer.set_value(Some(store.offer_reload("Recipe updated", reload)));
            }
        } else {
            withdraw();
        }
    });
    on_cleanup(withdraw);

    // What a request written during this cook is about: the recipe at the
    // revision and scale being cooked, and the step that is open.
    let ask_about = Callback::new(move |()| {
        let found = session.with_untracked(|session| {
            let session = session.as_ref()?;
            let recipe = store.recipe(&session.recipe_id)?;
            let state = state.get_untracked();
            let doc = store.session_doc(session, &state).map(|(doc, _)| doc);
            let step = doc.as_ref().and_then(|doc| {
                let ix = Index::build(doc);
                chosen
                    .get_untracked()
                    .filter(|id| ix.step(id).is_some())
                    .or_else(|| state.current_step(doc, None))
                    .and_then(|id| ix.step(&id).map(|step| (id.clone(), step.action.clone())))
            });
            let about = match &step {
                Some((_, action)) => format!(
                    "About {}, at the step \u{201c}{action}\u{201d}.",
                    recipe.title
                ),
                None => format!("About {}.", recipe.title),
            };
            Some(Ask {
                origin: omni_api::fond::RequestOrigin::Cook,
                recipe_id: Some(session.recipe_id.clone()),
                revision: Some(state.revision),
                scale: Some(session.scale),
                step: step.map(|(id, _)| id),
                about,
            })
        });
        found.unwrap_or_else(Ask::library)
    });

    if session.get_untracked().is_none() {
        spawn_local(async move { store.refresh_session(id).await });
    }

    let send = move |kind: CookEventKind| store.cook_event(&session_id.get_value(), kind);
    let recipe_route = move || {
        session
            .with_untracked(|s| s.as_ref().map(|s| s.recipe_id.clone()))
            .map_or(Route::Library, Route::Recipe)
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
    let discard = move |_| {
        let after = recipe_route();
        send(CookEventKind::Abandon {});
        discarding.set(false);
        finishing.set(false);
        leave_discarding.set(false);
        leaving.set(false);
        replace(store, after);
        store.show_toast("Cook discarded", None);
    };

    view! {
        <main class="page cook">
            <header class="bar">
                <button class="icon-btn" aria-label="Leave cook mode" data-testid="leave" on:click=move |_| {
                    // Only a cook in progress has anything to keep or discard.
                    if basis.with_untracked(Option::is_some) && !finished.get_untracked() {
                        leave_discarding.set(false);
                        leaving.set(true);
                    } else {
                        back(store, recipe_route());
                    }
                }>{icon("close")}</button>
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
                <button class="icon-btn" aria-label="More" on:click=move |_| menu.set(true)>{icon("more")}</button>
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
                let Some((doc, scale, recipe_id)) = basis.get() else {
                    return view! {
                        <div class="empty">
                            <p>"This cook session is not on this phone."</p>
                            <Link to=Route::Library class="btn">"Go to the library"</Link>
                        </div>
                    }
                    .into_any();
                };
                let ctx = screen(doc, scale, recipe_id);
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
                // A step cell opens its sheet; an input is crossed off.
                let tapped = Callback::new(move |cell: Cell| match cell.kind {
                    CellKind::Step | CellKind::Setup => {
                        sheet_step.set(cell.step);
                        step_open.set(true);
                    }
                    CellKind::Leaf | CellKind::Ref => ctx.toggle(&cell.key),
                });
                let held = Callback::new(move |cell: Cell| ctx.toggle_step(&cell.step, true));
                // As in the strip above the tabs, a done step's timer is not shown.
                let timers = Callback::new(move |cell: Cell| {
                    ctx.state.with(|state| {
                        state
                            .timers
                            .get(&cell.step)
                            .copied()
                            .filter(|_| !state.checked.contains(&cell.step))
                    })
                });
                let tab_button = move |this: Tab, label: &'static str| view! {
                    <button role="tab" class:on=move || tab.get() == this aria-selected=move || (tab.get() == this).to_string()
                        on:click=move |_| {
                            tab.set(this);
                            // Gather is a stop on the way, not a way to read the recipe.
                            match this {
                                Tab::Steps => prefs::set_recipe_view(RecipeView::Steps),
                                Tab::Chart => prefs::set_recipe_view(RecipeView::Chart),
                                Tab::Gather => {}
                            }
                        }>{label}</button>
                };
                view! {
                    <nav class="seg wide tabs" role="tablist">
                        {tab_button(Tab::Gather, "Gather")}
                        {tab_button(Tab::Steps, "Steps")}
                        {tab_button(Tab::Chart, "Chart")}
                    </nav>
                    <div class="scroller" data-testid="scroller"><div class="scroller-in">
                    {move || {
                        state
                            .with(|state| state.notes.clone())
                            .into_iter()
                            .map(|note| view! { <p class="session-note">{note}</p> })
                            .collect_view()
                    }}
                    {move || match tab.get() {
                        Tab::Gather => view! { <section class="tab gather-tab">{gather(ctx)}</section> }.into_any(),
                        Tab::Steps => view! { <section class="tab steps-tab">{meanwhile_offers(ctx)}{step_card(ctx)}</section> }.into_any(),
                        Tab::Chart => view! {
                            // A press on the chart is never the start of a selection or a menu.
                            <section class="tab charts live" data-testid="chart"
                                on:selectstart=move |event| event.prevent_default()
                                on:contextmenu=move |event| event.prevent_default()>
                                <Chart doc=ctx.doc.get_value() factor=scale mark=marks on_tap=tapped leaf_taps=true
                                    on_hold=held timer=timers/>
                                <p class="hint">"Tap a step for details. Press and hold it until it fills to mark it done, and again to undo. Tap an ingredient to cross it off."</p>
                            </section>
                        }
                        .into_any(),
                    }}
                    </div></div>
                    {dock(ctx)}
                }
                .into_any()
            }}
        </main>
        <Sheet open=menu title="Cooking">
            <div class="menu">{kitchen_items(menu, asking)}</div>
        </Sheet>
        <AskSheet open=asking about=ask_about/>
        <Sheet open=choosing_amount title="Change an amount">
            <div class="menu">
                {move || amount_choices
                    .get()
                    .into_iter()
                    .map(|(ingredient, name)| view! {
                        <button data-testid="change-amount" on:click=move |_| {
                            if let Some(recipe) = recipe_id.get() {
                                amount_target.set(Some(AmountTarget {
                                    recipe,
                                    ingredient: ingredient.clone(),
                                    session: Some(session_id.get_value()),
                                }));
                                choosing_amount.set(false);
                                amount_open.set(true);
                            }
                        }>{name}</button>
                    })
                    .collect_view()}
            </div>
        </Sheet>
        <Sheet open=notes_open title="Recipe notes">
            <p class="muted">"Notes are kept with the recipe."</p>
            // Built when opened, so the field is sized to its text on screen.
            {move || {
                notes_open
                    .get()
                    .then(|| recipe_id.get().map(|id| view! { <Notes id=id heading=false/> }))
                    .flatten()
            }}
        </Sheet>
        <AmountSheet open=amount_open target=amount_target/>
        <Sheet open=step_open title=Signal::derive(move || {
            let step = sheet_step.get();
            basis.with(|basis| basis.as_ref().map(|(doc, ..)| step_label(doc, &step))).unwrap_or_default()
        })>
            // Built while open: the Steps tab's card has the same controls.
            {move || {
                if !step_open.get() {
                    return None;
                }
                let (doc, scale, recipe_id) = basis.get()?;
                Some(step_sheet(screen(doc, scale, recipe_id)))
            }}
        </Sheet>
        <Sheet open=leaving title="Leave this cook?">
            <Show
                when=move || leave_discarding.get()
                fallback=move || view! {
                    <div class="menu">
                        <button data-testid="leave-keep" on:click=move |_| {
                            leaving.set(false);
                            back(store, recipe_route());
                        }>
                            "Keep for later"
                            <small>"It stays under Cooking now."</small>
                        </button>
                        <button class="danger" data-testid="leave-discard" on:click=move |_| leave_discarding.set(true)>
                            "Discard this cook"
                            <small>"Nothing is logged."</small>
                        </button>
                        <button data-testid="leave-cancel" on:click=move |_| leaving.set(false)>"Cancel"</button>
                    </div>
                }
            >
                <p class="muted">"Nothing from this cook is kept, and it does not count as cooked."</p>
                <div class="sheet-actions">
                    <button class="btn" on:click=move |_| leave_discarding.set(false)>"Back"</button>
                    <button class="btn danger" data-testid="leave-discard-confirm" on:click=discard>"Discard"</button>
                </div>
            </Show>
        </Sheet>
        <Sheet open=finishing title="Finish cooking">
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
            <div class="discard">
                <Show
                    when=move || discarding.get()
                    fallback=move || view! {
                        <button class="btn ghost small" data-testid="discard" on:click=move |_| discarding.set(true)>
                            "Discard this cook"
                        </button>
                    }
                >
                    <p class="muted">"Nothing from this cook is kept, and it does not count as cooked."</p>
                    <div class="sheet-actions">
                        <button class="btn" on:click=move |_| discarding.set(false)>"Keep cooking"</button>
                        <button class="btn danger" data-testid="discard-confirm" on:click=discard>"Discard"</button>
                    </div>
                </Show>
            </div>
        </Sheet>
    }
}
